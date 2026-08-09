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

mod classification;
mod markdown;
#[cfg(feature = "rendering")]
mod rendering;

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
    warning_sink: crate::extractors::warnings::WarningSink,
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

/// Scan raw file bytes for candidate ObjStm positions.
///
/// Each hit is `(object_number, byte_offset_of_N_G_obj_header)`. We look
/// for the shape `N G obj ... /Type /ObjStm` within a small window after
/// each object header so that the caller can then `load_uncompressed_object`
/// at exactly that offset without parsing the whole file body.
///
/// The scan is intentionally tolerant: it doesn't require `/Type`
/// `/ObjStm` to be separated by whitespace (many producers write
/// `/Type/ObjStm`), doesn't anchor on any particular position within the
/// header, and doesn't rely on xref entries being correct — which is the
/// whole point of the recovery path it serves.
fn find_objstm_candidates(content: &[u8]) -> Vec<(u32, u64)> {
    const DICT_PEEK_BYTES: usize = 2048;
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < content.len() {
        let valid_start = pos == 0
            || content[pos - 1] == b'\n'
            || content[pos - 1] == b'\r'
            || content[pos - 1] == b' ';
        if !valid_start || !content[pos].is_ascii_digit() {
            pos += 1;
            continue;
        }
        let header_start = pos;

        // Parse N (object number)
        let num_start = pos;
        while pos < content.len() && content[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos >= content.len() || content[pos] != b' ' {
            pos = header_start + 1;
            continue;
        }
        let obj_num: u32 = match std::str::from_utf8(&content[num_start..pos])
            .ok()
            .and_then(|s| s.parse().ok())
        {
            Some(n) => n,
            None => {
                pos = header_start + 1;
                continue;
            }
        };
        pos += 1;

        // Parse G (generation)
        let gen_start = pos;
        while pos < content.len() && content[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos >= content.len() || content[pos] != b' ' {
            pos = header_start + 1;
            continue;
        }
        if std::str::from_utf8(&content[gen_start..pos])
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .is_none()
        {
            pos = header_start + 1;
            continue;
        }
        pos += 1;

        // Require literal "obj"
        if pos + 3 > content.len() || &content[pos..pos + 3] != b"obj" {
            pos = header_start + 1;
            continue;
        }

        // Peek up to DICT_PEEK_BYTES ahead for `/Type` followed (after
        // optional whitespace) by `/ObjStm`. We don't decompress — the
        // ObjStm dict header is always uncompressed plaintext even when
        // the stream body is Flate-encoded.
        let window_end = (pos + DICT_PEEK_BYTES).min(content.len());
        let window = &content[pos..window_end];
        if contains_objstm_marker(window) {
            out.push((obj_num, header_start as u64));
        }

        pos = header_start + 1;
    }
    out
}

fn contains_objstm_marker(window: &[u8]) -> bool {
    // Tolerant match: find `/Type` then allow optional whitespace before `/ObjStm`.
    let mut i = 0;
    while i + 5 <= window.len() {
        if &window[i..i + 5] == b"/Type" {
            let mut j = i + 5;
            while j < window.len()
                && (window[j] == b' '
                    || window[j] == b'\t'
                    || window[j] == b'\r'
                    || window[j] == b'\n')
            {
                j += 1;
            }
            if j + 7 <= window.len() && &window[j..j + 7] == b"/ObjStm" {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Append ink names declared by `Separation` and `DeviceN` colour spaces
/// in `cs_dict` to `out`. Reserved colorants `/All` and `/None` (§8.6.6.4)
/// are skipped. Caller is responsible for deduping across multiple calls.
///
/// When `doc` is `Some`, indirect references inside each colour-space array
/// (e.g. a DeviceN whose names list is `4 0 R` rather than inline) are
/// resolved. Tools that hand-build inline arrays and don't need indirection
/// resolution can pass `None`.
///
/// Used by both [`PdfDocument::get_page_inks`] and
/// [`PdfDocument::get_page_inks_deep`] so the per-colorant rules live in
/// exactly one place.
fn extract_inks_from_color_space_dict(
    cs_dict: &std::collections::HashMap<String, Object>,
    doc: Option<&PdfDocument>,
    out: &mut Vec<String>,
) {
    let mut visited: std::collections::HashSet<ObjectRef> = std::collections::HashSet::new();
    for cs_def in cs_dict.values() {
        collect_inks_from_color_space(cs_def, doc, out, &mut visited, 0);
    }
}

/// Inner walker — surfaces inks from a single colour-space definition.
/// Factored out of [`extract_inks_from_color_space_dict`] so the
/// Pattern arm can recurse into its underlying colour space without
/// requiring a synthetic single-entry dict.
///
/// **Cycle handling:** the Pattern arm recurses into the underlying
/// colour space (§8.7.3.1). A self-referential array such as
/// `5 0 obj [/Pattern 5 0 R]` would otherwise blow the stack, so
/// indirect references are de-duplicated via `visited` (keyed on
/// `ObjectRef`) and total depth is capped at `MAX_RECURSION_DEPTH`
/// — the same backstop used by [`PdfDocument::walk_form_xobject_tree_for_inks`].
fn collect_inks_from_color_space(
    cs_def: &Object,
    doc: Option<&PdfDocument>,
    out: &mut Vec<String>,
    visited: &mut std::collections::HashSet<ObjectRef>,
    depth: u32,
) {
    if depth >= MAX_RECURSION_DEPTH {
        return;
    }
    let deref = |obj: &Object| -> Object {
        match (obj.as_reference(), doc) {
            (Some(r), Some(d)) => d.load_object(r).unwrap_or_else(|_| obj.clone()),
            _ => obj.clone(),
        }
    };

    let arr = match cs_def.as_array() {
        Some(a) => a,
        None => return,
    };
    if arr.len() < 2 {
        return;
    }
    let cs_type = match arr.first().and_then(Object::as_name) {
        Some(n) => n,
        None => return,
    };
    match cs_type {
        "Pattern" => {
            // ISO 32000-1 §8.7.3.1: a Pattern colour space's
            // optional second array element is the underlying
            // colour space (uncoloured Tiling carries the
            // underlying space's tints). Recurse so a Pattern
            // with /Separation or /DeviceN underlying surfaces
            // the spot colorants for plate allocation.
            //
            // Guard against self-referential cycles (e.g.
            // `5 0 obj [/Pattern 5 0 R]`): an indirect underlying
            // ref is recorded in `visited`; a repeat hit terminates
            // the recursion silently.
            if let Some(r) = arr[1].as_reference() {
                if !visited.insert(r) {
                    return;
                }
            }
            let underlying = deref(&arr[1]);
            collect_inks_from_color_space(&underlying, doc, out, visited, depth + 1);
        }
        "Separation" => {
            // §8.6.6.2: [/Separation /InkName /AlternateCS /TintTransform].
            // The name slot is usually inline but resolve indirects for safety.
            let name_obj = deref(&arr[1]);
            if let Some(ink) = name_obj.as_name() {
                if ink != "All" && ink != "None" {
                    out.push(ink.to_string());
                }
            }
        }
        "DeviceN" => {
            // §8.6.6.5: [/DeviceN <names-array> /AlternateCS /TintTransform <attrs>].
            // The names array is commonly emitted as an indirect reference
            // when the same colorant set is shared across multiple DeviceN
            // spaces; resolve before unpacking the names.
            let names_obj = match arr.get(1) {
                Some(o) => deref(o),
                None => return,
            };
            // ISO 32000-1 §8.6.6.5 / Table 73: the optional 5th array
            // element is the attributes dictionary. When its `/Process`
            // sub-dictionary declares a `/Components` array, those names
            // are PROCESS colorants (riding the page's process plates),
            // not spot inks. The same rule applies whether the attrs
            // dict's `/Subtype` is `/DeviceN` (the default, PDF 1.6) or
            // `/NChannel` (PDF 1.7 stricter subtype) — §8.6.6.5 names the
            // /Process key on both subtypes. Build the process-name set
            // here so the colorants loop can filter against it.
            let process_names: std::collections::HashSet<String> = arr
                .get(4)
                .map(&deref)
                .as_ref()
                .and_then(Object::as_dict)
                .and_then(|attrs| attrs.get("Process"))
                .map(&deref)
                .as_ref()
                .and_then(Object::as_dict)
                .and_then(|proc_dict| proc_dict.get("Components"))
                .map(&deref)
                .as_ref()
                .and_then(Object::as_array)
                .map(|comps| {
                    comps
                        .iter()
                        .filter_map(|o| o.as_name().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(inks) = names_obj.as_array() {
                for ink_obj in inks {
                    if let Some(ink) = ink_obj.as_name() {
                        if ink != "All" && ink != "None" && !process_names.contains(ink) {
                            out.push(ink.to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Per-page MCID action computed from the
/// [`crate::structure::ActualTextIndex`].
///
/// Drives every consumer of struct-tree-scope `/ActualText`
/// (`extract_text`'s structure-order assembler, the raw-span applier,
/// and the ordered-span applier). The map is computed once per page
/// from the cached `ActualTextIndex` plus the visibility / MC-scope
/// filters; consumers then dispatch per MCID without re-walking the
/// structure tree.
#[derive(Debug, Clone)]
pub(crate) enum ActualTextAction {
    /// Replace this MCID's span text with the supplied string AND drop
    /// subsequent spans / MCIDs in the same consecutive-replacement
    /// run. Assigned to exactly one MCID per emitting run: the first
    /// visible MCID that is not exempted by MC-scope-wins.
    EmitAndSuppress(std::sync::Arc<str>),
    /// Suppress the raw glyphs for this MCID without emitting anything.
    /// Used for run continuations after the run's emission MCID, for
    /// suppress-only entries (non-first-page coverage of a multi-page
    /// ActualText scope), and for MCIDs in a fully-hidden run.
    Suppress,
}

impl PdfDocument {
    #[cfg(test)]
    pub(crate) fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let reader = PdfReader::Memory(BufReader::new(Cursor::new(data)));
        Self::open_from_reader(
            reader,
            DEFAULT_OBJECT_CACHE_MAX_BYTES,
            DEFAULT_XOBJECT_CACHE_MAX_ENTRIES,
        )
    }

    pub(crate) fn from_file_with_limits(
        mut file: File,
        object_cache_bytes: usize,
        xobject_cache_entries: usize,
    ) -> Result<Self> {
        file.seek(SeekFrom::Start(0))?;
        Self::open_from_reader(
            PdfReader::File(BufReader::new(file)),
            object_cache_bytes,
            xobject_cache_entries,
        )
    }

    fn open_from_reader(
        mut reader: PdfReader,
        object_cache_bytes: usize,
        xobject_cache_entries: usize,
    ) -> Result<Self> {
        // Parse header with lenient mode by default (handle PDFs with binary prefixes)
        let (major, minor, header_offset) = parse_header(&mut reader, true)?;
        let version = (major, minor);

        // Whether the xref table below came from a full-file reconstruction
        // scan (vs. a parsed xref). Used to pre-seed the object-scan cache so
        // a later miss doesn't rescan the whole file a second time (#572).
        let mut xref_reconstructed = false;
        // SYNTHETIC objects a recovery invented (a rebuilt Catalog / page-tree
        // root for a truncated file). They have no byte offset, so they are
        // seeded into the object cache after the document is built. Empty in the
        // ordinary case.
        let mut synthetic_objects: Vec<(ObjectRef, Object)> = Vec::new();

        // Try to parse xref table normally
        let (mut xref, trailer) = match Self::try_open_regular(&mut reader) {
            Ok((xref, trailer)) => {
                // Success with regular parsing
                // However, if the xref is suspiciously small (< 5 entries), it's likely corrupted
                // Try reconstruction to get a complete table
                if xref.is_empty() {
                    log::warn!(
                        "Regular xref parsing succeeded but table is empty, attempting reconstruction"
                    );
                    xref_reconstructed = true;
                    let (x, t, syn) = Self::try_reconstruct_xref(&mut reader)?;
                    synthetic_objects = syn;
                    (x, t)
                } else {
                    // A valid xref can have any number of entries (§7.5.4).
                    // Small xrefs (e.g. portfolio PDFs with 3-4 objects) are perfectly
                    // normal — don't trigger expensive full-file reconstruction for them.
                    (xref, trailer)
                }
            }
            Err(e) => {
                log::warn!(
                    "Regular xref parsing failed: {}, attempting reconstruction",
                    e
                );

                // Fall back to xref reconstruction
                match Self::try_reconstruct_xref(&mut reader) {
                    Ok((reconstructed_xref, reconstructed_trailer, syn)) => {
                        log::info!("Successfully reconstructed xref table");
                        xref_reconstructed = true;
                        synthetic_objects = syn;
                        (reconstructed_xref, reconstructed_trailer)
                    }
                    Err(recon_err) => {
                        log::error!("XRef reconstruction also failed: {}", recon_err);
                        return Err(e); // Return original error
                    }
                }
            }
        };

        // If PDF header is not at byte 0 (garbage-prepended), xref offsets may need adjustment.
        // The xref offsets are relative to the original PDF start, but file positions are
        // shifted by header_offset bytes.
        if header_offset > 0 {
            // Probe an object to decide whether xref offsets are off by
            // header_offset. Prefer /Root (common case), but the probe MUST
            // be seek-validatable: `validate_object_at_offset` returns true
            // for *compressed* entries without seeking, so a /Root that
            // lives in an object stream would falsely report "no shift
            // needed" and leave every uncompressed offset wrong. Use /Root
            // only when its entry is in-use + uncompressed; otherwise (no
            // /Root — issue #509 — or a compressed /Root) fall back to the
            // first in-use uncompressed object.
            let probe = get_root_ref_from_trailer(&trailer)
                .filter(|r| {
                    xref.get(r.id).is_some_and(|e| {
                        e.in_use && e.entry_type == crate::xref::XRefEntryType::Uncompressed
                    })
                })
                .or_else(|| first_in_use_uncompressed(&xref));
            if let Some(probe_ref) = probe {
                if !validate_object_at_offset(&mut reader, &xref, probe_ref) {
                    log::info!(
                        "Probe object {} not loadable at xref offset, adjusting all offsets by header_offset={}",
                        probe_ref.id, header_offset
                    );
                    xref.shift_offsets(header_offset);
                }
            }
        }

        // Validate the /Root catalog is actually loadable. If not, the xref data is
        // corrupt despite parsing successfully — fall back to reconstruction.
        let (xref, trailer) = if !validate_root_loadable(&mut reader, &xref, &trailer) {
            log::warn!(
                "Root object not loadable after xref parse, falling back to xref reconstruction"
            );
            match Self::try_reconstruct_xref(&mut reader) {
                Ok((x, t, syn)) => {
                    xref_reconstructed = true;
                    synthetic_objects = syn;
                    (x, t)
                }
                Err(_) => (xref, trailer), // Use original if reconstruction also fails
            }
        } else {
            (xref, trailer)
        };

        // #572: a reconstruction scan already located every uncompressed
        // "N G obj" in the file, so a later scan_for_object full-file rescan
        // (on the first object miss) would find nothing new — it just repeats
        // the work, the ~25 s "first extract_text" cost on corrupt-xref
        // polyglots. Pre-seed the scan-offset cache from the reconstructed
        // table so that first miss is O(1). Only do this when reconstructed:
        // a normal (parsed) xref may be legitimately partial, and there the
        // full scan is the intended recovery path.
        let prepopulated_scan: Option<HashMap<u32, u64>> = if xref_reconstructed {
            Some(
                xref.all_object_numbers()
                    .filter_map(|id| {
                        xref.get(id).and_then(|e| {
                            (e.in_use && e.entry_type == crate::xref::XRefEntryType::Uncompressed)
                                .then_some((id, e.offset))
                        })
                    })
                    .collect(),
            )
        } else {
            None
        };

        // Note: Encryption initialization was originally lazy, but decode_stream_with_encryption
        // only has &self access which prevents initialization.
        // We now initialize eagerly to ensure the handler is ready when needed.
        let document = Self {
            reader: Mutex::new(reader),
            load_lock: Mutex::new(()),
            version,
            xref,
            trailer,
            object_cache: Mutex::new(BoundedObjectCache::new(object_cache_bytes)),
            encryption_handler: Mutex::new(None),
            encrypt_dict_ref: Mutex::new(None),
            options: ParserOptions::default(),
            header_offset,
            font_cache: Mutex::new(BoundedEntryCache::new(512)),
            font_set_cache: Mutex::new(BoundedEntryCache::new(256)),
            font_fingerprint_cache: Mutex::new(BoundedEntryCache::new(256)),
            font_name_set_cache: Mutex::new(BoundedEntryCache::new(256)),
            font_identity_cache: Mutex::new(BoundedEntryCache::new(512)),
            font_id_hash_cache: Mutex::new(HashMap::new()),
            structure_tree_cache: Mutex::new(None),
            structure_content_cache: Mutex::new(None),
            actualtext_index_cache: Mutex::new(None),
            mc_actualtext_mcids: Mutex::new(HashMap::new()),
            table_elements_cache: Mutex::new(None),
            page_cache: Mutex::new(HashMap::new()),
            page_cache_populated: AtomicBool::new(false),
            scanned_object_offsets: Mutex::new(prepopulated_scan),
            objstm_recovery_done: Mutex::new(false),
            image_xobject_cache: Mutex::new(HashSet::new()),
            xobject_text_free_cache: Mutex::new(HashSet::new()),
            xobject_stream_cache: Mutex::new(HashMap::new()),
            xobject_stream_cache_bytes: AtomicUsize::new(0),
            xobject_spans_cache: Mutex::new(BoundedEntryCache::new(xobject_cache_entries)),
            form_xobject_images_cache: Mutex::new(BoundedEntryCache::new(xobject_cache_entries)),
            page_content_cache: Mutex::new(BoundedEntryCache::new(64)),
            page_spans_cache: Mutex::new(BoundedEntryCache::new(8)),
            page_chars_cache: Mutex::new(BoundedEntryCache::new(8)),
            running_artifact_signatures: Mutex::new(None),
            article_threads_cache: Mutex::new(None),
            output_intent_cmyk_profile_cache: Mutex::new(None),
            accumulated_warnings: Mutex::new(Vec::new()),
            warning_sink: crate::extractors::warnings::WarningSink::new(),
        };

        // Seed any SYNTHETIC recovery objects (a Catalog / page-tree root rebuilt
        // for a truncated file) into the object cache. They have no byte offset,
        // so `load_object` - which checks the cache before the xref - is the only
        // way to reach them. Done before encryption init so the /Root resolves.
        if !synthetic_objects.is_empty() {
            let mut cache = document.object_cache.lock_or_recover();
            for (obj_ref, obj) in synthetic_objects {
                cache.insert(obj_ref, obj);
            }
        }

        // Initialize encryption immediately
        if let Err(e) = document.ensure_encryption_initialized() {
            log::error!("Failed to initialize encryption: {}", e);
            // We continue anyway, as it might just be an unsupported security handler
            // and maybe we can still read parts of the file (or fail later)
        }

        Ok(document)
    }

    /// Try to open the PDF using regular xref parsing.
    fn try_open_regular<R: Read + Seek>(reader: &mut R) -> Result<(CrossRefTable, Object)> {
        // Find xref table offset
        let xref_offset = find_xref_offset(reader)?;

        // Parse xref table
        let xref = parse_xref(reader, xref_offset)?;

        // Get trailer dictionary
        let trailer = if let Some(trailer_dict) = xref.trailer() {
            // XRef stream: trailer is already in the xref table
            Object::Dictionary(trailer_dict.clone())
        } else {
            // Traditional xref: parse trailer separately
            reader.seek(SeekFrom::Start(xref_offset))?;
            parse_trailer(reader)?
        };

        Ok((xref, trailer))
    }

    /// Try to reconstruct the xref table by scanning the file. The third tuple
    /// element is any SYNTHETIC objects (a rebuilt Catalog / page-tree root for a
    /// truncated file) the caller must seed into the object cache - empty in the
    /// ordinary case.
    fn try_reconstruct_xref<R: Read + Seek>(
        reader: &mut R,
    ) -> Result<(CrossRefTable, Object, Vec<(ObjectRef, Object)>)> {
        crate::xref_reconstruction::reconstruct_xref(reader)
    }

    /// Initialize encryption handler lazily if PDF is encrypted.
    ///
    /// PDF Spec: Section 7.6.1 - Encryption dictionary in trailer
    ///
    /// This checks for the /Encrypt entry in the trailer, loads it if it's a
    /// reference, and creates an encryption handler. It automatically attempts
    /// to authenticate with an empty password (common for PDFs with default encryption).
    ///
    /// This is called lazily the first time we need to decrypt something, after
    /// the document is fully constructed and can load objects.
    fn ensure_encryption_initialized(&self) -> Result<()> {
        // Already initialized?
        if self.encryption_handler.lock_or_recover().is_some() {
            return Ok(());
        }

        // Clone what we need from trailer to avoid borrow conflicts
        let (encrypt_ref, file_id) = {
            let trailer_dict = match self.trailer.as_dict() {
                Some(d) => d,
                None => return Ok(()), // No trailer dict, no encryption
            };

            // Check for /Encrypt entry
            let encrypt_entry = match trailer_dict.get("Encrypt") {
                Some(obj) => obj,
                None => {
                    log::debug!("PDF is not encrypted (no /Encrypt entry)");
                    return Ok(());
                }
            };

            // Clone the encrypt entry (we'll load it outside this block)
            let encrypt_ref = encrypt_entry.clone();

            // Get file ID (required for encryption key derivation)
            let file_id = match trailer_dict.get("ID") {
                Some(Object::Array(arr)) => {
                    if let Some(first_id) = arr.first() {
                        if let Some(id_bytes) = first_id.as_string() {
                            id_bytes.to_vec()
                        } else {
                            log::warn!(
                                "Invalid /ID array entry (not a string), using empty file ID"
                            );
                            vec![]
                        }
                    } else {
                        log::warn!("Empty /ID array, using empty file ID");
                        vec![]
                    }
                }
                _ => {
                    log::warn!("Missing or invalid /ID entry in trailer, using empty file ID");
                    vec![]
                }
            };

            (encrypt_ref, file_id)
        }; // End of borrow scope

        // Now load the encrypt object (dereference if needed)
        let encrypt_obj = match encrypt_ref {
            Object::Dictionary(_) => encrypt_ref,
            Object::Reference(obj_ref) => {
                log::debug!(
                    "Loading /Encrypt object reference {} {}",
                    obj_ref.id,
                    obj_ref.gen
                );
                // Remember which object holds the /Encrypt dict so its own
                // strings are skipped during per-object string decryption.
                *self.encrypt_dict_ref.lock_or_recover() = Some(obj_ref);
                self.load_object(obj_ref)?
            }
            _ => {
                return Err(Error::InvalidPdf(format!(
                    "Invalid /Encrypt entry type: {}",
                    encrypt_ref.type_name()
                )));
            }
        };

        // Resolve any indirect references within the encrypt dictionary.
        // Some PDFs store /O, /U, /V, /R, /P as indirect references (e.g., `7 0 R`).
        let encrypt_obj = if let Some(dict) = encrypt_obj.as_dict() {
            let mut resolved_dict = dict.clone();
            for value in resolved_dict.values_mut() {
                if let Object::Reference(obj_ref) = value {
                    match self.load_object(*obj_ref) {
                        Ok(resolved) => *value = resolved,
                        Err(e) => {
                            log::warn!("Failed to resolve indirect ref in /Encrypt dict: {}", e);
                        }
                    }
                }
            }
            Object::Dictionary(resolved_dict)
        } else {
            encrypt_obj
        };

        // Create encryption handler with the file_id we extracted above
        let mut handler = EncryptionHandler::new(&encrypt_obj, file_id)?;

        // Try to authenticate with empty password (common default)
        match handler.authenticate(b"") {
            Ok(true) => {
                log::info!("Successfully authenticated with empty password");
            }
            Ok(false) => {
                log::warn!("PDF is encrypted and requires a password");
                self.push_warning("PDF is encrypted and requires a password".to_string());
            }
            Err(e) => {
                log::error!("Failed to initialize encryption: {}", e);
                return Err(e);
            }
        }

        *self.encryption_handler.lock_or_recover() = Some(handler);
        Ok(())
    }

    /// Decode stream data with encryption support.
    ///
    /// This is a helper method that decodes stream data using the PDF's encryption handler
    /// if the document is encrypted. It automatically handles object-specific key derivation.
    ///
    /// # Arguments
    ///
    /// * `stream_obj` - The stream object to decode
    /// * `obj_ref` - The object reference (for encryption key derivation)
    ///
    /// # Returns
    ///
    /// The decoded (and decrypted if needed) stream data.
    ///
    /// # PDF Spec Reference
    ///
    /// ISO 32000-1:2008, Section 7.6.2 - Streams must be decrypted BEFORE applying filters.
    pub(crate) fn decode_stream_with_encryption(
        &self,
        stream_obj: &Object,
        obj_ref: ObjectRef,
    ) -> Result<Vec<u8>> {
        if matches!(stream_obj, Object::Null) {
            return Ok(Vec::new());
        }

        // Per ISO 32000-2:2020 Section 7.6.3, object streams (/Type /ObjStm)
        // and cross-reference streams (/Type /XRef) shall NOT be encrypted.
        // Skip decryption for these stream types to avoid AES block-size errors
        // on data that was never encrypted in the first place.
        let is_unencrypted_stream_type = if let Object::Stream { dict, .. } = stream_obj {
            dict.get("Type")
                .and_then(|t| t.as_name())
                .map(|name| name == "ObjStm" || name == "XRef")
                .unwrap_or(false)
        } else {
            false
        };

        let handler_ref = self.encryption_handler.lock_or_recover();
        if let Some(handler) = handler_ref.as_ref() {
            if is_unencrypted_stream_type {
                // These stream types are never encrypted per spec
                drop(handler_ref);
                return stream_obj.decode_stream_data();
            }
            // Create decryption closure for this specific object
            let decrypt_fn = |data: &[u8]| -> Result<Vec<u8>> {
                handler.decrypt_stream(data, obj_ref.id, obj_ref.gen as u32)
            };
            stream_obj.decode_stream_data_with_decryption(
                Some(&decrypt_fn),
                obj_ref.id,
                obj_ref.gen as u32,
            )
        } else {
            drop(handler_ref);
            // No encryption, use regular decoding
            stream_obj.decode_stream_data()
        }
    }

    /// Check if the PDF is encrypted.
    ///
    /// Returns `true` if the PDF has an `/Encrypt` entry in its trailer,
    /// regardless of whether it has been authenticated.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// if doc.is_encrypted() {
    ///     println!("PDF is encrypted");
    /// }
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn is_encrypted(&self) -> bool {
        // Check if encryption handler is already initialized
        if self.encryption_handler.lock_or_recover().is_some() {
            return true;
        }
        // Check trailer for /Encrypt entry without initializing
        self.trailer
            .as_dict()
            .and_then(|d| d.get("Encrypt"))
            .is_some()
    }

    /// Whether content extraction is permitted right now — `true` if the
    /// PDF is unencrypted, or encrypted and successfully authenticated.
    ///
    /// Cheap, side-effect-free preflight for the auto-extraction
    /// classifier (#517): lets it emit
    /// [`ReasonCode::EncryptedNoExtractPermission`](crate::extractors::auto::ReasonCode)
    /// gracefully instead of attempting extraction and erroring.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        // Fail closed: if encryption init errors (malformed / unsupported
        // `/Encrypt`), the document IS encrypted but we cannot have
        // authenticated it — a security preflight must report `false`
        // here, not `true` (PR #519 review). Only when init succeeds
        // (incl. the trivial unencrypted case) do we trust the guard.
        if self.ensure_encryption_initialized().is_err() {
            return false;
        }
        !self.is_encrypted_and_unauthenticated()
    }

    /// Check if the PDF is encrypted but has NOT been successfully authenticated.
    ///
    /// This returns `true` when the document requires a password that has not
    /// yet been provided. Extraction methods use this to return a clear error
    /// instead of silently producing empty output.
    fn is_encrypted_and_unauthenticated(&self) -> bool {
        if let Some(handler) = self.encryption_handler.lock_or_recover().as_ref() {
            !handler.is_authenticated()
        } else {
            // Handler not yet initialized — check if /Encrypt exists
            // If it does, we don't know auth state yet, so return false
            // (ensure_encryption_initialized will handle it)
            false
        }
    }

    /// Guard that returns `Err(Error::EncryptedPdf)` if the PDF is encrypted
    /// and not authenticated. Call this at the top of extraction methods.
    fn require_authenticated(&self) -> Result<()> {
        // Make sure encryption is initialized first
        self.ensure_encryption_initialized()?;
        if self.is_encrypted_and_unauthenticated() {
            return Err(Error::EncryptedPdf);
        }
        Ok(())
    }

    /// True once the empty user password has been tried and the document is
    /// still locked. Text extraction degrades to empty output in this case
    /// (matching pdftotext/PyMuPDF) rather than erroring; `page_count` and
    /// write paths keep using [`Self::require_authenticated`].
    fn is_encrypted_unreadable(&self) -> bool {
        let _ = self.ensure_encryption_initialized();
        self.is_encrypted_and_unauthenticated()
    }

    /// Get the PDF version.
    ///
    /// Returns a tuple (major, minor) representing the PDF version.
    /// For example, PDF 1.7 returns (1, 7).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let (major, minor) = doc.version();
    /// println!("PDF version: {}.{}", major, minor);
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn version(&self) -> (u8, u8) {
        self.version
    }

    /// Get a reference to the trailer dictionary.
    ///
    /// The trailer dictionary contains important document metadata including:
    /// - /Root: Reference to the catalog dictionary
    /// - /Info: Reference to the document info dictionary (optional)
    /// - /Size: Number of entries in the cross-reference table
    /// - /Encrypt: Encryption dictionary (if encrypted)
    /// - /ID: File identifier array
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let trailer = doc.trailer();
    /// if let Some(dict) = trailer.as_dict() {
    ///     if let Some(info_ref) = dict.get("Info") {
    ///         println!("Document has an Info dictionary");
    ///     }
    /// }
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn trailer(&self) -> &Object {
        &self.trailer
    }

    /// Return every object ID known to this document.
    ///
    /// Unions the cross-reference table with any object IDs that were
    /// recovered from compressed object streams (which may not have an
    /// explicit xref entry). The result is sorted and deduplicated so
    /// callers can iterate once and write each object exactly once.
    ///
    /// Used by `DocumentEditor::write_full_to_writer` to sweep any
    /// objects that were not reached during the shallow page-tree
    /// traversal (e.g. embedded font sub-objects such as
    /// `DescendantFonts`, `FontFile2`, `ToUnicode`, `FontDescriptor`).
    pub fn all_object_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.xref.all_object_numbers().collect();
        for r in self.object_cache.lock_or_recover().keys() {
            ids.push(r.id);
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Return references to every leaf page, in document order, with a single
    /// page-tree traversal.
    ///
    /// Replaces the O(n²) pattern of calling [`get_page_ref`] in a 0..n loop:
    /// each `get_page_ref(i)` walks the tree from the root and stops at the
    /// i-th leaf, so collecting all n refs walks 1+2+...+n nodes.
    ///
    /// Optimised for the common flat-tree case: when a `Pages` node's
    /// `Count` matches `Kids.len()`, every kid is a leaf and we can take
    /// the references straight from the array without loading each leaf.
    /// Only when the tree is multi-level do we recurse and load child nodes.
    pub(crate) fn all_page_refs(&self) -> Result<Vec<ObjectRef>> {
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;
        let pages_ref = catalog_dict
            .get("Pages")
            .and_then(|p| p.as_reference())
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?;

        let mut out: Vec<ObjectRef> = Vec::new();
        let mut visited: HashSet<ObjectRef> = HashSet::new();
        self.collect_page_refs(pages_ref, &mut out, &mut visited)?;
        Ok(out)
    }

    fn collect_page_refs(
        &self,
        node_ref: ObjectRef,
        out: &mut Vec<ObjectRef>,
        visited: &mut HashSet<ObjectRef>,
    ) -> Result<()> {
        if !visited.insert(node_ref) {
            return Ok(());
        }
        let node = self.load_object(node_ref)?;
        let dict = match node.as_dict() {
            Some(d) => d,
            None => return Ok(()),
        };

        let kids = match dict.get("Kids").and_then(|k| k.as_array()) {
            Some(k) => k,
            None => {
                // Leaf reached (no /Kids — assume Page).
                out.push(node_ref);
                return Ok(());
            }
        };

        // Fast path: flat subtree — every kid is a leaf when /Count == kids.len().
        let count = dict.get("Count").and_then(|c| c.as_integer()).unwrap_or(-1);
        if count >= 0 && (count as usize) == kids.len() {
            for kid in kids {
                if let Some(kid_ref) = kid.as_reference() {
                    out.push(kid_ref);
                }
            }
            return Ok(());
        }

        // Mixed tree — recurse into each kid.
        for kid in kids {
            if let Some(kid_ref) = kid.as_reference() {
                self.collect_page_refs(kid_ref, out, visited)?;
            }
        }
        Ok(())
    }

    /// Scan the file to find an object by its header.
    ///
    /// This is a fallback method used when an object is not in the xref table
    /// but is referenced by critical structures (like Pages from Catalog).
    /// Some PDFs have incomplete xref tables that are missing entries for
    /// objects that actually exist in the file.
    fn scan_for_object(&self, obj_ref: ObjectRef) -> Result<u64> {
        // Check cached scan results first
        {
            let scan_cache = self.scanned_object_offsets.lock_or_recover();
            if let Some(offsets) = scan_cache.as_ref() {
                if let Some(&offset) = offsets.get(&obj_ref.id) {
                    return Ok(offset);
                }
                return Err(Error::ObjectNotFound(obj_ref.id, obj_ref.gen));
            }
        }

        // First xref miss: scan the entire file once and build a complete offset map
        log::info!(
            "Building object offset map from file scan (triggered by object {} {})",
            obj_ref.id,
            obj_ref.gen
        );

        let mut content = Vec::new();
        {
            // Hold one guard for seek+read to prevent split-lock race (#398 Race A).
            let mut reader = self.reader.lock_or_recover();
            reader.seek(SeekFrom::Start(0))?;
            reader.read_to_end(&mut content)?;
        }

        let mut offsets = HashMap::new();

        // Scan for all "N G obj" patterns in the file
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
                    offsets.entry(obj_num).or_insert(start as u64);
                    pos = after_obj;
                    continue;
                }
            }
            // Reset pos to just after the start to avoid infinite loop
            pos = start + 1;
        }

        log::info!("File scan found {} objects", offsets.len());

        let result = offsets.get(&obj_ref.id).copied();
        *self.scanned_object_offsets.lock_or_recover() = Some(offsets);

        match result {
            Some(offset) => Ok(offset),
            None => Err(Error::ObjectNotFound(obj_ref.id, obj_ref.gen)),
        }
    }

    /// One-time sweep over every known object stream (`/Type /ObjStm`),
    /// used to recover from xref tables that mis-mark compressed objects as
    /// free.
    ///
    /// Some PDF producers emit an xref where a compressed object's slot is
    /// type 0 (free) instead of type 2 (compressed → stream#). The object
    /// is physically stored inside an `ObjStm`, but `scan_for_object` can't
    /// find it because it has no standalone `N G obj` marker in the body.
    ///
    /// The recovery: iterate every uncompressed candidate, peek at the
    /// dictionary, and for those that are `/Type /ObjStm`, parse the stream
    /// and cache everything inside (overwriting any stale `Object::Null`
    /// entries from earlier free-entry short-circuits).
    ///
    /// Runs at most once per document — guarded by `objstm_recovery_done`.
    /// Cost is amortised across every recovered object.
    fn recover_from_object_streams(&self) {
        use crate::objstm::parse_object_stream_with_decryption;

        {
            let done = self.objstm_recovery_done.lock_or_recover();
            if *done {
                return;
            }
        }

        log::debug!("Sweeping object streams to recover xref-flagged-free objects");

        // Find ObjStm candidates by raw pattern search in the file body.
        //
        // Why not iterate xref entries here: the xref is precisely what we
        // don't trust in this recovery path — its offsets may be wrong
        // its type tags may be lying about what each slot contains. A raw
        // search for `N G obj ... /Type /ObjStm` finds every object stream
        // the producer actually wrote, independent of how the xref
        // describes them.
        //
        // Only flip `objstm_recovery_done` after we finish the scan+parse
        // pass; a transient seek/read failure should leave the flag unset
        // so a later retry can still attempt recovery.
        let file_bytes = {
            let mut r = self.reader.lock_or_recover();
            if r.seek(SeekFrom::Start(0)).is_err() {
                return;
            }
            let mut buf = Vec::new();
            if r.read_to_end(&mut buf).is_err() {
                return;
            }
            buf
        };

        let candidates = find_objstm_candidates(&file_bytes);

        let mut objstms_found = 0usize;
        let mut recovered = 0usize;
        for (stream_obj_num, offset) in &candidates {
            let stream_ref = ObjectRef::new(*stream_obj_num, 0);
            let stream_obj = match self.load_uncompressed_object(stream_ref, *offset) {
                Ok(obj) => obj,
                Err(_) => continue,
            };

            let is_objstm = stream_obj
                .as_dict()
                .and_then(|d| d.get("Type"))
                .and_then(|t| t.as_name())
                .is_some_and(|n| n == "ObjStm");
            if !is_objstm {
                continue;
            }
            objstms_found += 1;

            // Parse the stream body. ISO 32000-2:2020 §7.6.3 says ObjStm
            // shall NOT be individually encrypted, so skip decryption here
            // — mirrors the default branch in `load_compressed_object`.
            let objects_map = match parse_object_stream_with_decryption(&stream_obj, None, 0, 0) {
                Ok(m) => m,
                Err(e) => {
                    log::debug!(
                        "Skipping ObjStm {} during recovery sweep (parse failed: {})",
                        stream_obj_num,
                        e
                    );
                    continue;
                }
            };

            let mut cache = self.object_cache.lock_or_recover();
            for (obj_num, object) in objects_map {
                let cache_ref = ObjectRef::new(obj_num, 0);
                // Only overwrite entries we'd otherwise have resolved to
                // Null (the free-entry short-circuit caches Null). Never
                // clobber a real object loaded through the normal path.
                match cache.get(&cache_ref) {
                    Some(Object::Null) | None => {
                        cache.insert(cache_ref, object);
                        recovered += 1;
                    }
                    _ => {}
                }
            }
        }

        log::debug!(
            "Object-stream recovery sweep: {} candidate positions, {} ObjStms, {} objects cached",
            candidates.len(),
            objstms_found,
            recovered
        );

        *self.objstm_recovery_done.lock_or_recover() = true;
    }

    /// Load an object by its reference.
    ///
    /// This function:
    /// 1. Checks the object cache first
    /// 2. If not cached, looks up the byte offset in the xref table
    /// 3. Seeks to that offset and parses the object
    /// 4. Caches the result for future access
    /// 5. If object not in xref but is critical, scans file for it
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The object reference is not in the xref table and file scan fails
    /// - The object is not in use (free object)
    /// - Seeking to the object offset fails
    /// - Parsing the object fails
    /// - A circular reference is detected
    /// - The recursion depth limit is exceeded
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # use pdf_oxide::object::ObjectRef;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let obj_ref = ObjectRef::new(1, 0);
    /// let obj = doc.load_object(obj_ref)?;
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn load_object(&self, obj_ref: ObjectRef) -> Result<Object> {
        log::debug!("Loading object {} gen {}", obj_ref.id, obj_ref.gen);

        // Check recursion depth (per-thread counter; no lock needed)
        {
            let depth = RECURSION_DEPTH.with(|d| *d.borrow());
            if depth >= MAX_RECURSION_DEPTH {
                log::error!(
                    "Recursion depth limit exceeded ({}) while loading object {} gen {}",
                    MAX_RECURSION_DEPTH,
                    obj_ref.id,
                    obj_ref.gen
                );
                return Err(Error::RecursionLimitExceeded(MAX_RECURSION_DEPTH));
            }
        }

        // Check for circular references (per-thread stack; concurrent threads
        // resolving the same object do NOT appear as a false cycle)
        if RESOLVING_STACK.with(|s| s.borrow().contains(&obj_ref)) {
            log::error!(
                "Circular reference detected for object {} gen {} (depth: {})",
                obj_ref.id,
                obj_ref.gen,
                RECURSION_DEPTH.with(|d| *d.borrow())
            );
            return Err(Error::CircularReference(obj_ref));
        }

        // Check cache first (warm path: fully parallel, no serialization).
        let cached_opt = self.object_cache.lock_or_recover().get(&obj_ref).cloned();
        if let Some(cached) = cached_opt {
            return Ok(cached);
        }

        // Cold path (#507): serialize uncached loads across threads so a
        // single logical load's many `reader` lock scopes are not
        // interleaved by another thread's load on the shared `BufReader`.
        // Acquire ONLY at the top-level entry (recursion depth 0); a
        // recursive call from this same thread (nested-ref resolution)
        // already holds the guard, so re-acquiring would self-deadlock —
        // skip it. Held for the remainder of this top-level resolution.
        let _load_guard = if RECURSION_DEPTH.with(|d| *d.borrow()) == 0 {
            let guard = self.load_lock.lock_or_recover();
            // Double-checked: another thread may have loaded and cached
            // this object while we were blocked on the guard.
            if let Some(cached) = self.object_cache.lock_or_recover().get(&obj_ref).cloned() {
                return Ok(cached);
            }
            Some(guard)
        } else {
            None
        };

        // Look up in xref table
        let entry = match self.xref.get(obj_ref.id) {
            Some(entry) => entry,
            None => {
                // Object not in xref table - try scanning the file as fallback
                // This handles PDFs with incomplete/corrupted xref tables
                let available: Vec<u32> = self.xref.entries.keys().copied().take(20).collect();
                log::warn!(
                    "Object {} not in xref table. Total entries: {}. First 20 objects: {:?}",
                    obj_ref.id,
                    self.xref.len(),
                    available
                );

                // Try to scan the file for this object
                match self.scan_for_object(obj_ref) {
                    Ok(offset) => {
                        // Found it! Load directly from this offset
                        log::info!(
                            "Successfully found object {} via file scan at offset {}",
                            obj_ref.id,
                            offset
                        );

                        // Mark as being resolved (per-thread cycle detection)
                        RESOLVING_STACK.with(|s| {
                            s.borrow_mut().insert(obj_ref);
                        });
                        RECURSION_DEPTH.with(|d| *d.borrow_mut() += 1);

                        // Load the object
                        let result = self.load_uncompressed_object(obj_ref, offset);

                        RECURSION_DEPTH.with(|d| *d.borrow_mut() -= 1);
                        RESOLVING_STACK.with(|s| {
                            s.borrow_mut().remove(&obj_ref);
                        });

                        return result;
                    }
                    Err(_) => {
                        // PDF Spec §7.3.10: missing object reference "shall be treated as null"
                        log::warn!("Object {} gen {} not found (xref + file scan failed), treating as Null per §7.3.10", obj_ref.id, obj_ref.gen);
                        self.object_cache
                            .lock_or_recover()
                            .insert(obj_ref, Object::Null);
                        return Ok(Object::Null);
                    }
                }
            }
        };

        log::debug!(
            "  → Found in xref: type={:?}, offset={}, gen={}, in_use={}",
            entry.entry_type,
            entry.offset,
            entry.generation,
            entry.in_use
        );

        // Check if object is in use
        if !entry.in_use {
            log::debug!(
                "Object {} is marked as free (not in use). This may be due to a corrupted xref table.",
                obj_ref.id
            );

            // xref flags the object free, but this may be xref corruption
            // rather than an actual deletion. Run two recovery paths before
            // falling back to §7.3.10's null. The branches below apply
            // uniformly for all object ids (critical low-numbered catalog
            // objects and page objects in the thousands); previously low
            // ids took a separate "fall through to loading logic" path
            // that silently hit the Free arm of the entry_type match
            // still ended up Null.
            //
            // Recovery path 1 — standalone `N G obj` marker in the file
            // body. `scan_for_object` builds a whole-file offset map once
            // per document and caches it, so the amortised cost is a
            // single O(filesize) pass no matter how many free-marked
            // objects we probe.
            if let Ok(scanned_offset) = self.scan_for_object(obj_ref) {
                log::debug!(
                    "Object {} marked free in xref but found in file scan at offset {}; recovering",
                    obj_ref.id,
                    scanned_offset
                );
                RESOLVING_STACK.with(|s| {
                    s.borrow_mut().insert(obj_ref);
                });
                RECURSION_DEPTH.with(|d| *d.borrow_mut() += 1);
                let result = self.load_uncompressed_object(obj_ref, scanned_offset);
                RECURSION_DEPTH.with(|d| *d.borrow_mut() -= 1);
                RESOLVING_STACK.with(|s| {
                    s.borrow_mut().remove(&obj_ref);
                });
                return result;
            }

            // Recovery path 2 — the object may be compressed inside a
            // `/Type /ObjStm`. Real-world producers have been seen to
            // mis-flag every compressed object's xref slot as free, so
            // sweep the object streams once and recheck the cache.
            self.recover_from_object_streams();
            if let Some(obj) = self.object_cache.lock_or_recover().get(&obj_ref).cloned() {
                if !matches!(obj, Object::Null) {
                    log::debug!("Object {} recovered from object-stream sweep", obj_ref.id);
                    return Ok(obj);
                }
            }

            // PDF Spec §7.3.10: free object treated as null
            log::warn!(
                "Free object {} gen {}, treating as Null per §7.3.10",
                obj_ref.id,
                obj_ref.gen
            );
            self.object_cache
                .lock_or_recover()
                .insert(obj_ref, Object::Null);
            return Ok(Object::Null);
        }

        // Mark as being resolved (per-thread cycle detection)
        RESOLVING_STACK.with(|s| {
            s.borrow_mut().insert(obj_ref);
        });
        RECURSION_DEPTH.with(|d| *d.borrow_mut() += 1);

        // Handle different entry types
        use crate::xref::XRefEntryType;
        let entry_type = entry.entry_type;
        let entry_offset = entry.offset;
        let entry_gen = entry.generation;
        let result = match entry_type {
            XRefEntryType::Compressed => {
                // Type 2 entry: object is in an object stream
                // entry.offset = stream object number
                // entry.generation = index within stream
                log::debug!(
                    "  → Compressed object in stream {}, index {}",
                    entry_offset,
                    entry_gen
                );
                self.load_compressed_object(obj_ref, entry_offset as u32, entry_gen)
            }
            XRefEntryType::Uncompressed => {
                // Type 1 entry: traditional uncompressed object
                log::debug!("  → Uncompressed object at offset {}", entry_offset);
                self.load_uncompressed_object(obj_ref, entry_offset)
            }
            XRefEntryType::Free => {
                // Free object - shouldn't happen since we check in_use above
                // PDF Spec §7.3.10: treat as null
                log::warn!(
                    "Object {} has type Free despite in_use=true, treating as Null",
                    obj_ref.id
                );
                self.object_cache
                    .lock_or_recover()
                    .insert(obj_ref, Object::Null);
                Ok(Object::Null)
            }
        };

        RECURSION_DEPTH.with(|d| *d.borrow_mut() -= 1);
        RESOLVING_STACK.with(|s| {
            s.borrow_mut().remove(&obj_ref);
        });

        result
    }

    /// Resolve references within an object recursively.
    ///
    /// This utility method resolves indirect references within an object,
    /// handling nested dictionaries and arrays up to a specified depth.
    /// Useful for processing complex PDF structures where properties
    /// may be stored as indirect references.
    ///
    /// # Arguments
    ///
    /// * `obj` - The object to resolve references within
    /// * `max_depth` - Maximum recursion depth to prevent infinite loops
    ///
    /// # Returns
    ///
    /// The object with all references resolved up to max_depth levels.
    /// If a reference cannot be resolved, it is left as-is.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// # let obj = doc.catalog()?;
    /// // Resolve all references in a dictionary up to 3 levels deep
    /// let resolved = doc.resolve_references(&obj, 3)?;
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn resolve_references(&self, obj: &Object, max_depth: usize) -> Result<Object> {
        if max_depth == 0 {
            return Ok(obj.clone());
        }

        match obj {
            Object::Reference(obj_ref) => {
                // Resolve the reference
                match self.load_object(*obj_ref) {
                    Ok(resolved) => {
                        // Recursively resolve within the resolved object
                        self.resolve_references(&resolved, max_depth - 1)
                    }
                    Err(e) => {
                        log::warn!("Failed to resolve reference {:?}: {}", obj_ref, e);
                        Ok(obj.clone()) // Return the unresolved reference
                    }
                }
            }

            Object::Dictionary(dict) => {
                // Resolve references within each value
                let mut resolved_dict = std::collections::HashMap::new();
                for (key, value) in dict.iter() {
                    let resolved_value = self.resolve_references(value, max_depth - 1)?;
                    resolved_dict.insert(key.clone(), resolved_value);
                }
                Ok(Object::Dictionary(resolved_dict))
            }

            Object::Array(arr) => {
                // Resolve references within each element
                let resolved_arr: Result<Vec<Object>> = arr
                    .iter()
                    .map(|item| self.resolve_references(item, max_depth - 1))
                    .collect();
                Ok(Object::Array(resolved_arr?))
            }

            // For all other types, just return a clone
            _ => Ok(obj.clone()),
        }
    }

    /// Resolve a single-level indirect reference (PDF spec §7.3.10).
    ///
    /// If `obj` is `Object::Reference(...)`, loads and returns the target object.
    /// For any other object type, returns a clone unchanged. This is the
    /// canonical way to handle "any value may be a direct or indirect reference"
    /// throughout the parser.
    fn resolve_obj_ref(&self, obj: &Object) -> Object {
        if let Some(obj_ref) = obj.as_reference() {
            match self.load_object(obj_ref) {
                Ok(resolved) => resolved,
                Err(e) => {
                    log::warn!("Failed to resolve indirect reference {:?}: {}", obj_ref, e);
                    obj.clone()
                }
            }
        } else {
            obj.clone()
        }
    }

    /// Peek at an XObject's /Subtype without loading the full object.
    /// Returns true if the XObject is a Form XObject, false if Image or unknown.
    /// For compressed objects or on any error, returns true (conservative — will load fully).
    pub fn is_form_xobject(&self, obj_ref: ObjectRef) -> bool {
        // Check negative cache first (known non-Form XObjects)
        {
            if self
                .image_xobject_cache
                .lock_or_recover()
                .contains(&obj_ref)
            {
                return false;
            }
        }

        // If already in object cache, check directly
        let cached_opt = self.object_cache.lock_or_recover().get(&obj_ref).cloned();
        if let Some(cached) = cached_opt {
            let is_form = cached
                .as_dict()
                .and_then(|d| d.get("Subtype"))
                .and_then(|s| s.as_name())
                == Some("Form");
            if !is_form {
                self.image_xobject_cache.lock_or_recover().insert(obj_ref);
            }
            return is_form;
        }

        // Look up in xref table
        let entry = match self.xref.get(obj_ref.id) {
            Some(e) => e,
            None => return true, // conservative fallback
        };

        // Only peek uncompressed objects — compressed ones require full load
        use crate::xref::XRefEntryType;
        if entry.entry_type != XRefEntryType::Uncompressed || !entry.in_use {
            return true; // conservative fallback
        }

        // Seek + read under a SINGLE lock guard. Splitting the seek
        // the read across two `self.reader.lock_or_recover()` acquisitions
        // is the #398 Race A split-lock bug (same one already fixed in
        // `load_uncompressed_object_impl`): a concurrent thread can
        // re-seek the shared reader between our seek() and read(), so we
        // read a garbage buffer for a different object. That surfaced as
        // a spurious `[1000] invalid PDF structure or content stream`
        // ParseError under concurrent `render_page_fit`.
        let offset = entry.offset;
        let mut buf = [0u8; 1024];
        let n = {
            let mut reader = self.reader.lock_or_recover();
            if reader.seek(SeekFrom::Start(offset)).is_err() {
                return true;
            }
            // Read enough bytes for the object header + dictionary (<1KB)
            match reader.read(&mut buf) {
                Ok(n) => n,
                Err(_) => return true,
            }
        };
        let data = &buf[..n];

        // Search for /Subtype in the buffer
        // Look for "/Subtype" followed by a name like "/Form" or "/Image"
        if let Some(pos) = data.windows(8).position(|w| w == b"/Subtype") {
            let after = &data[pos + 8..];
            // Skip whitespace
            let trimmed = after
                .iter()
                .position(|&b| b != b' ' && b != b'\t' && b != b'\r' && b != b'\n');
            if let Some(start) = trimmed {
                let name_data = &after[start..];
                if name_data.starts_with(b"/Form") {
                    return true;
                }
                // Image, PS, or anything else — not a Form
                self.image_xobject_cache.lock_or_recover().insert(obj_ref);
                return false;
            }
        }

        // /Subtype not found in first 1KB — conservative fallback
        true
    }

    /// Load an uncompressed object (Type 1 xref entry).
    fn load_uncompressed_object(&self, obj_ref: ObjectRef, offset: u64) -> Result<Object> {
        self.load_uncompressed_object_impl(obj_ref, offset, false)
    }

    /// Promote labels in rowspan-sparse columns so they sort at the top
    /// of their data-row block instead of landing mid-group.
    ///
    /// A "label" here is a span in an X-cluster that contains far fewer
    /// spans than the most populous X-cluster (i.e., it spans multiple
    /// rows of the adjacent data column). Labels are typically vertically
    /// centred in their block, so a strict Y sort places them between
    /// the rows they describe. This post-processor detects the pattern
    /// and rewrites each label's effective sort Y to sit just above the
    /// topmost data row it visually covers.
    ///
    /// Data rows are partitioned between adjacent labels at the midpoint
    /// of their Y coordinates (nearest-label assignment). The topmost
    /// data row in a label's partition becomes the anchor for promotion.
    ///
    /// Nothing is mutated if there are no sparse columns or not enough
    /// data rows to confidently infer row-grouping (min 6 rows in the
    /// dense reference column).
    /// Identify span indices that look like multi-row-spanning labels —
    /// sparse-X-column spans whose Y values sit inside the data Y range
    /// of the dense columns on the page. These are the same spans that
    /// `reorder_rowspan_labels` would promote to the top of their row
    /// block, except this function returns them **before** the spatial
    /// table detector's retain filter has a chance to drop them from
    /// the flow span list.
    ///
    /// The retain filter in `extract_text_with_options` removes every
    /// span whose bbox is contained in a detected table's bbox. On CJK
    /// reference-data PDFs the test-name label column is
    /// narrow and vertically centred within each multi-row data block,
    /// so its spans are inside the table bbox and would be dropped
    /// without replacement — the spatial table extractor does not emit
    /// these labels as `TableCell`s either. Preserving the identified
    /// labels through the retain filter lets `reorder_rowspan_labels`
    /// promote them to their proper reading-order position alongside
    /// the surviving flow spans.
    ///
    /// Returns a `HashSet` of indices into the provided `spans` slice.
    /// Callers must use the returned indices **before** any reordering
    /// or retention mutates the slice.
    pub(crate) fn identify_multi_row_labels(
        spans: &[crate::layout::TextSpan],
    ) -> std::collections::HashSet<usize> {
        use std::collections::{BTreeSet, HashMap as StdHashMap, HashSet};

        let mut out: HashSet<usize> = HashSet::new();
        if spans.len() < 10 {
            return out;
        }

        // Cluster by X proximity (15pt gap threshold) — same heuristic
        // as `reorder_rowspan_labels`.
        let mut by_x: Vec<usize> = (0..spans.len()).collect();
        by_x.sort_by(|&a, &b| crate::utils::safe_float_cmp(spans[a].bbox.x, spans[b].bbox.x));
        const X_GAP: f32 = 15.0;
        let mut columns: Vec<Vec<usize>> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        let mut last_x = f32::NEG_INFINITY;
        for &idx in &by_x {
            let x = spans[idx].bbox.x;
            if !cur.is_empty() && x - last_x > X_GAP {
                columns.push(std::mem::take(&mut cur));
            }
            cur.push(idx);
            last_x = x;
        }
        if !cur.is_empty() {
            columns.push(cur);
        }
        if columns.len() < 2 {
            return out;
        }

        let max_count = columns.iter().map(|c| c.len()).max().unwrap_or(0);
        if max_count < 6 {
            return out;
        }

        // Sort columns by span count descending to pick the dense clusters.
        let mut col_order: Vec<usize> = (0..columns.len()).collect();
        col_order.sort_by(|&a, &b| columns[b].len().cmp(&columns[a].len()));
        let dense_cols_count = columns.iter().filter(|c| c.len() * 2 > max_count).count();

        let band_of = |y: f32| (y / crate::utils::ROW_BAND_TOLERANCE_PT).round() as i32;
        let data_bands: BTreeSet<i32> = if dense_cols_count >= 3 {
            let top: Vec<&Vec<usize>> = col_order.iter().take(3).map(|&i| &columns[i]).collect();
            let mut support: StdHashMap<i32, usize> = StdHashMap::new();
            for col in &top {
                let bands: HashSet<i32> = col.iter().map(|&i| band_of(spans[i].bbox.y)).collect();
                for b in bands {
                    *support.entry(b).or_insert(0) += 1;
                }
            }
            support
                .into_iter()
                .filter(|(_, c)| *c >= 3)
                .map(|(b, _)| b)
                .collect()
        } else if dense_cols_count == 2 {
            let a: HashSet<i32> = columns[col_order[0]]
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect();
            let b: HashSet<i32> = columns[col_order[1]]
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect();
            a.intersection(&b).copied().collect()
        } else {
            columns[col_order[0]]
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect()
        };

        if data_bands.len() < 4 {
            return out;
        }

        let band_pt = crate::utils::ROW_BAND_TOLERANCE_PT;
        let data_top = (*data_bands.iter().next_back().unwrap() as f32) * band_pt + band_pt / 2.0;
        let data_bot = (*data_bands.iter().next().unwrap() as f32) * band_pt - band_pt / 2.0;

        // Collect sparse-column spans that sit inside the data Y range
        // and belong to a column with >= 2 members in that range.
        for col in &columns {
            if col.len() < 2 || col.len() * 2 >= max_count {
                continue;
            }
            let in_data: Vec<usize> = col
                .iter()
                .copied()
                .filter(|&i| {
                    let y = spans[i].bbox.y;
                    y > data_bot && y < data_top
                })
                .collect();
            if in_data.len() >= 2 {
                out.extend(in_data);
            }
        }

        out
    }

    pub(crate) fn reorder_rowspan_labels(spans: &mut Vec<crate::layout::TextSpan>) {
        use std::collections::HashMap;

        if spans.len() < 10 {
            return;
        }

        // Cluster by X proximity (15pt gap threshold). Walk spans ordered
        // by left edge; start a new cluster whenever the gap exceeds the
        // threshold.
        let mut by_x: Vec<usize> = (0..spans.len()).collect();
        by_x.sort_by(|&a, &b| crate::utils::safe_float_cmp(spans[a].bbox.x, spans[b].bbox.x));
        const X_GAP: f32 = 15.0;
        let mut columns: Vec<Vec<usize>> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        let mut last_x = f32::NEG_INFINITY;
        for &idx in &by_x {
            let x = spans[idx].bbox.x;
            if !cur.is_empty() && x - last_x > X_GAP {
                columns.push(std::mem::take(&mut cur));
            }
            cur.push(idx);
            last_x = x;
        }
        if !cur.is_empty() {
            columns.push(cur);
        }
        if columns.len() < 2 {
            return;
        }

        // Max column size is our reference for "dense".
        let max_count = columns.iter().map(|c| c.len()).max().unwrap_or(0);
        if max_count < 6 {
            return;
        }

        // Sort columns by span count descending so we can pick the top
        // dense cluster for anchor detection.
        let mut col_order: Vec<usize> = (0..columns.len()).collect();
        col_order.sort_by(|&a, &b| columns[b].len().cmp(&columns[a].len()));

        // A column is "dense" when it holds a strict majority of the
        // most populous column's spans. Pages with multiple dense data
        // columns (three or more) let us derive the data-row range by
        // intersecting their Y bands — headers and sub-headers populate
        // only a subset of columns at their Y and fall out.
        let dense_cols_count = columns.iter().filter(|c| c.len() * 2 > max_count).count();

        // Most populous column, used for anchor Y lookups regardless.
        let dense_col = &columns[col_order[0]];
        let mut dense_ys: Vec<f32> = dense_col.iter().map(|&i| spans[i].bbox.y).collect();
        dense_ys.sort_by(|a, b| crate::utils::safe_float_cmp(*b, *a));

        // Compute the set of Y bands that count as "data". When several
        // dense columns are available, require a band to have support in
        // the top three; otherwise fall back to the single dense column's
        // own Y values.
        let band_of = |y: f32| (y / crate::utils::ROW_BAND_TOLERANCE_PT).round() as i32;
        use std::collections::{BTreeSet, HashMap as StdHashMap, HashSet};

        let data_bands: BTreeSet<i32> = if dense_cols_count >= 3 {
            let top: Vec<&Vec<usize>> = col_order.iter().take(3).map(|&i| &columns[i]).collect();
            let mut support: StdHashMap<i32, usize> = StdHashMap::new();
            for col in &top {
                let bands: HashSet<i32> = col.iter().map(|&i| band_of(spans[i].bbox.y)).collect();
                for b in bands {
                    *support.entry(b).or_insert(0) += 1;
                }
            }
            support
                .into_iter()
                .filter(|(_, c)| *c >= 3)
                .map(|(b, _)| b)
                .collect()
        } else if dense_cols_count == 2 {
            let a: HashSet<i32> = columns[col_order[0]]
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect();
            let b: HashSet<i32> = columns[col_order[1]]
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect();
            a.intersection(&b).copied().collect()
        } else {
            dense_col
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect()
        };

        if data_bands.len() < 4 {
            return;
        }
        let band_pt = crate::utils::ROW_BAND_TOLERANCE_PT;
        let data_top = (*data_bands.iter().next_back().unwrap() as f32) * band_pt + band_pt / 2.0;
        let data_bot = (*data_bands.iter().next().unwrap() as f32) * band_pt - band_pt / 2.0;

        // Y-bands occupied by the dense column. Genuine rowspan labels are
        // vertically centred *between* data rows, so their Y-band must NOT
        // appear in this set. Spans whose Y aligns with the dense column are
        // line-continuation text on the same logical line, not labels.
        let dense_bands: HashSet<i32> = dense_col
            .iter()
            .map(|&i| band_of(spans[i].bbox.y))
            .collect();

        // Numbered reference/bibliography lists render the leading marker
        // ("1.", "2.", "3.") in a narrow column to the left of the body
        // text. That marker column is sparse (one per entry) and its markers
        // sit between body rows, so they look exactly like rowspan labels to
        // the heuristic below — but they are NOT: each number belongs to its
        // own entry and promoting them scrambles the reference order. Detect
        // the pattern (>=3 numbered markers sharing a tight left-edge cluster
        // and spread down >=3 distinct rows = a vertical numbered list) and
        // exclude those markers from label promotion.
        let is_numbered_marker = |i: usize| -> bool {
            let t = spans[i].text.trim_start();
            let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
            (1..=3).contains(&digits) && t[digits..].starts_with(['.', ')'])
        };
        let numbered_excluded: HashSet<usize> = {
            let markers: Vec<usize> = (0..spans.len())
                .filter(|&i| is_numbered_marker(i))
                .collect();
            if markers.len() >= 3 {
                let mut xs: Vec<f32> = markers.iter().map(|&i| spans[i].bbox.x).collect();
                xs.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
                let median_x = xs[xs.len() / 2];
                let cluster: Vec<usize> = markers
                    .iter()
                    .copied()
                    .filter(|&i| (spans[i].bbox.x - median_x).abs() <= 6.0)
                    .collect();
                let rows: HashSet<i32> =
                    cluster.iter().map(|&i| band_of(spans[i].bbox.y)).collect();
                if cluster.len() >= 3 && rows.len() >= 3 {
                    cluster.into_iter().collect()
                } else {
                    HashSet::new()
                }
            } else {
                HashSet::new()
            }
        };

        // Collect "label" candidates: spans that sit in a "sparse"
        // column — one that holds meaningfully fewer spans than the
        // most populous column. A candidate only qualifies when it
        // sits strictly inside the data Y range AND the sparse column
        // it belongs to has at least two entries inside that range —
        // single-span sparse cells are almost always stray annotations,
        // not labels.
        let mut labels: Vec<usize> = Vec::new();
        for col in &columns {
            if col.len() < 2 || col.len() * 2 >= max_count {
                continue;
            }
            let in_data: Vec<usize> = col
                .iter()
                .copied()
                .filter(|&i| {
                    let y = spans[i].bbox.y;
                    // Exclude spans on the same Y-band as the dense column:
                    // those are line-continuation text, not rowspan labels.
                    // Also exclude numbered-list markers (reference numbers),
                    // which would otherwise be hoisted out of reading order.
                    y > data_bot
                        && y < data_top
                        && !dense_bands.contains(&band_of(y))
                        && !numbered_excluded.contains(&i)
                })
                .collect();
            if in_data.len() >= 2 {
                labels.extend(in_data);
            }
        }
        if labels.is_empty() {
            return;
        }
        labels.sort_by(|&a, &b| crate::utils::safe_float_cmp(spans[b].bbox.y, spans[a].bbox.y));

        // Labels that sit at near-identical Y values almost always
        // annotate the same logical row block (e.g. a test-name in the
        // "name" column alongside a unit "×10⁹/L" in the "unit" column,
        // both vertically centred in the same 6-row group). Cluster
        // labels by Y proximity so each logical block is promoted as a
        // unit.
        const CLUSTER_GAP: f32 = 10.0;
        let mut clusters: Vec<Vec<usize>> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        let mut last_y = f32::NAN;
        for &idx in &labels {
            let y = spans[idx].bbox.y;
            if !cur.is_empty() && (last_y - y).abs() > CLUSTER_GAP {
                clusters.push(std::mem::take(&mut cur));
            }
            cur.push(idx);
            last_y = y;
        }
        if !cur.is_empty() {
            clusters.push(cur);
        }
        let cluster_ys: Vec<f32> = clusters
            .iter()
            .map(|c| c.iter().map(|&i| spans[i].bbox.y).sum::<f32>() / c.len() as f32)
            .collect();

        // For each cluster, compute the midpoint partition boundaries
        // against its immediate neighbour clusters and find the topmost
        // dense-column Y that falls inside the partition. Promote every
        // member of the cluster to the same anchor so they sort together
        // at the top of their row block.
        let mut promoted: HashMap<usize, f32> = HashMap::new();
        for (k, cluster) in clusters.iter().enumerate() {
            let c_y = cluster_ys[k];
            let upper = if k > 0 {
                (cluster_ys[k - 1] + c_y) / 2.0
            } else {
                f32::INFINITY
            };
            let lower = if k + 1 < clusters.len() {
                (c_y + cluster_ys[k + 1]) / 2.0
            } else {
                f32::NEG_INFINITY
            };
            let upper_clamped = upper.min(data_top);
            let lower_clamped = lower.max(data_bot - 1.0);
            let mut anchor = f32::NEG_INFINITY;
            for &y in &dense_ys {
                if y <= upper_clamped && y > lower_clamped && y > anchor {
                    anchor = y;
                }
            }
            if anchor.is_finite() {
                for &i in cluster {
                    promoted.insert(i, anchor + 1.0);
                }
            }
        }
        if promoted.is_empty() {
            return;
        }

        // Re-sort spans using the promoted Ys for labels and actual Ys
        // for everything else. Keep the row-aware comparator so the
        // ordering stays consistent with the rest of the pipeline.
        let mut order: Vec<usize> = (0..spans.len()).collect();
        order.sort_by(|&a, &b| {
            let ya = promoted.get(&a).copied().unwrap_or(spans[a].bbox.y);
            let yb = promoted.get(&b).copied().unwrap_or(spans[b].bbox.y);
            crate::utils::row_aware_span_cmp(ya, spans[a].bbox.x, yb, spans[b].bbox.x)
        });
        let reordered: Vec<crate::layout::TextSpan> =
            order.into_iter().map(|i| spans[i].clone()).collect();
        *spans = reordered;
    }

    /// Recursively decrypt every `Object::String` inside `obj` using the
    /// per-object key derived from `obj_num`/`gen_num`. Streams are left
    /// untouched — they are decrypted lazily at read time through
    /// `decode_stream_with_encryption`. The `/Encrypt` dictionary itself
    /// must never be passed to this function; its strings are key material,
    /// not ciphertext.
    ///
    /// Per ISO 32000-1:2008 §7.6.2, strings inside encrypted-document
    /// objects are individually encrypted with the standard encryption
    /// algorithm. Parsed string tokens hold raw ciphertext and must be
    /// decrypted before downstream consumers (widget text, form field
    /// values, outlines, document info) can read them.
    fn decrypt_strings_in_object(
        handler: &EncryptionHandler,
        obj: &mut Object,
        obj_num: u32,
        gen_num: u32,
    ) {
        match obj {
            Object::String(bytes) => match handler.decrypt_string(bytes, obj_num, gen_num) {
                Ok(decrypted) => *bytes = decrypted,
                Err(e) => {
                    log::debug!(
                        "String decryption failed for object {} {}: {}",
                        obj_num,
                        gen_num,
                        e
                    );
                }
            },
            Object::Array(items) => {
                for item in items {
                    Self::decrypt_strings_in_object(handler, item, obj_num, gen_num);
                }
            }
            Object::Dictionary(dict) => {
                for value in dict.values_mut() {
                    Self::decrypt_strings_in_object(handler, value, obj_num, gen_num);
                }
            }
            Object::Stream { dict, .. } => {
                // Stream *data* is decrypted separately in
                // `decode_stream_with_encryption`. Its dict may still
                // contain encrypted strings (e.g., /Metadata).
                for value in dict.values_mut() {
                    Self::decrypt_strings_in_object(handler, value, obj_num, gen_num);
                }
            }
            _ => {}
        }
    }

    /// Implementation with recursion guard to prevent infinite loops.
    fn load_uncompressed_object_impl(
        &self,
        obj_ref: ObjectRef,
        offset: u64,
        already_corrected: bool,
    ) -> Result<Object> {
        // --- Phase 1: read the object header under a single lock guard ---
        // Holding one guard for seek+read prevents the split-lock race (#398 Race A)
        // where a concurrent thread can re-seek the shared BufReader between our
        // seek() and read_until() calls.
        let (header_bytes, full_header) = {
            let mut reader = self.reader.lock_or_recover();
            reader.seek(SeekFrom::Start(offset))?;

            // Read bytes for object header (e.g., "1 0 obj")
            let mut header_bytes = Vec::new();
            let bytes_read = reader.read_until(b'\n', &mut header_bytes)?;

            if bytes_read == 0 {
                let msg = format!("Unexpected EOF while reading object {} header", obj_ref.id);
                log::warn!("{}", msg);
                // also push into structured sink so
                // callers can retrieve as data via flatten_warnings.
                self.push_structured_warning(crate::extractors::warnings::Warning {
                    category: crate::extractors::warnings::WarningCategory::EofPremature,
                    page: None,
                    message: msg,
                    spec_section: Some("7.5"),
                });
                return Err(Error::UnexpectedEof);
            }

            let line = String::from_utf8_lossy(&header_bytes);

            // Issue #45: Handle multi-line object headers
            let mut full_header = line.to_string();
            let max_header_lines = 5;
            let mut lines_read = 1;

            while !has_standalone_obj_keyword(&full_header) && lines_read < max_header_lines {
                let mut next_bytes = Vec::new();
                let next_read = reader.read_until(b'\n', &mut next_bytes)?;
                if next_read == 0 {
                    break;
                }
                let next_line = String::from_utf8_lossy(&next_bytes);
                full_header.push(' ');
                full_header.push_str(&next_line);
                lines_read += 1;
            }
            // Reader guard drops here — before any recursive fallback calls.
            (header_bytes, full_header)
        };

        // Verify object header format
        // Split by whitespace to handle various formats (single-line or multi-line)
        let parts: Vec<&str> = full_header.split_whitespace().collect();

        // Find standalone "obj" keyword (not "endobj")
        let obj_pos = parts
            .iter()
            .position(|&p| p == "obj" || (p.starts_with("obj") && !p.starts_with("endobj")));

        // Validate object header has proper format: <id> <gen> obj
        let obj_pos = match obj_pos {
            Some(pos) if pos >= 2 => pos,
            _ => {
                // Only try backwards search once to prevent infinite recursion
                if !already_corrected {
                    // xref offset might be incorrect (pointing to object body instead of header)
                    // Try searching backwards for the object header
                    log::debug!(
                        "No object header at offset {}, searching backwards for object {} {} obj",
                        offset,
                        obj_ref.id,
                        obj_ref.gen
                    );

                    if let Ok(corrected_offset) = self.find_object_header_backwards(obj_ref, offset)
                    {
                        log::info!(
                            "Found object header at offset {} (xref said {})",
                            corrected_offset,
                            offset
                        );
                        return self.load_uncompressed_object_impl(obj_ref, corrected_offset, true);
                    }
                }

                log::warn!(
                    "Malformed object header at offset {}: {}",
                    offset,
                    full_header.trim()
                );
                return Err(Error::ParseError {
                    offset: offset as usize,
                    reason: format!("Expected object header, found: {}", full_header.trim()),
                });
            }
        };

        let _obj_pos = obj_pos;

        // Parse the object number and generation from header. If either
        // fails to parse as a number, the xref-reported offset is pointing
        // into the middle of a previous object's tail (e.g. xref says 12345
        // but the real `N G obj` header starts at 12348 because three bytes
        // of CR/LF/terminator got mis-accounted for by the producer — a
        // pattern seen in the wild). Fall back to the whole-file scan
        // cache: if scan recorded a different offset for this id, retry
        // from there before giving up.
        let obj_num_parsed = parts[0].parse::<u32>();
        let gen_num_parsed = parts[1].parse::<u16>();
        if !already_corrected && (obj_num_parsed.is_err() || gen_num_parsed.is_err()) {
            if let Ok(scan_offset) = self.scan_for_object(obj_ref) {
                if scan_offset != offset {
                    log::debug!(
                        "Header parse failed at xref offset {} (parts[0]={:?}); retrying at scan-reported offset {}",
                        offset,
                        parts[0],
                        scan_offset
                    );
                    return self.load_uncompressed_object_impl(obj_ref, scan_offset, true);
                }
            }
        }
        let obj_num: u32 = obj_num_parsed.map_err(|_| Error::ParseError {
            offset: offset as usize,
            reason: format!("Invalid object number in header: {}", parts[0]),
        })?;
        let gen_num: u16 = gen_num_parsed.map_err(|_| Error::ParseError {
            offset: offset as usize,
            reason: format!("Invalid generation number in header: {}", parts[1]),
        })?;

        // Verify object reference matches (warn but don't fail on mismatch)
        if obj_num != obj_ref.id || gen_num != obj_ref.gen {
            log::warn!(
                "Object reference mismatch at offset {}: expected {} {} obj, found {} {} obj",
                offset,
                obj_ref.id,
                obj_ref.gen,
                obj_num,
                gen_num
            );
        }

        // Check if there's content after "obj" on the same line
        // Some PDFs have "N G obj\n<<..." while others have "N G obj<<..." on one line
        let mut data = Vec::new();

        // Find where "obj" ends in the original bytes
        // We need to include anything after "obj" in the header line
        if let Some(obj_keyword_pos) = header_bytes.windows(3).position(|w| w == b"obj") {
            let after_obj_pos = obj_keyword_pos + 3; // "obj" is 3 bytes

            // Skip whitespace after "obj"
            let mut content_start = after_obj_pos;
            while content_start < header_bytes.len()
                && (header_bytes[content_start] == b' '
                    || header_bytes[content_start] == b'\t'
                    || header_bytes[content_start] == b'\r')
            {
                content_start += 1;
            }

            // If there's a newline, skip it (normal case: "N G obj\n")
            // If there's content (like "<<"), include it (malformed case: "N G obj<<...")
            if content_start < header_bytes.len() && header_bytes[content_start] != b'\n' {
                // There's content on the same line after "obj" - include it
                data.extend_from_slice(&header_bytes[content_start..]);
                log::debug!(
                    "Object {} has content after 'obj' on header line ({} bytes)",
                    obj_ref.id,
                    header_bytes.len() - content_start
                );
            }
        }

        // --- Phase 2: read body under a single lock guard (#398 Race A) ---
        // Use byte limit instead of line count — large uncompressed streams can have
        // hundreds of thousands of short lines (e.g., vector path drawing commands).
        const MAX_BYTES: usize = 100 * 1024 * 1024; // 100 MB safety limit

        {
            let mut reader = self.reader.lock_or_recover();
            loop {
                let mut chunk = Vec::new();
                let bytes_read = reader.read_until(b'\n', &mut chunk)?;

                if data.len() > MAX_BYTES {
                    log::warn!(
                        "Object {} exceeded maximum byte limit ({} bytes), truncating",
                        obj_ref.id,
                        MAX_BYTES
                    );
                    break;
                }

                if bytes_read == 0 {
                    let msg = format!(
                        "Unexpected EOF while reading object {} (no endobj found after {} bytes)",
                        obj_ref.id,
                        data.len()
                    );
                    log::warn!("{}", msg);
                    // structured-warnings sink.
                    self.push_structured_warning(crate::extractors::warnings::Warning {
                        category: crate::extractors::warnings::WarningCategory::EofPremature,
                        page: None,
                        message: msg,
                        spec_section: Some("7.5"),
                    });
                    // Don't fail - try to parse what we have
                    break;
                }

                // Check if we reached endobj
                if chunk.contains(&b'e') {
                    // Find "endobj" in the chunk (working with bytes, not chars)
                    if let Some(endobj_pos) = find_substring(&chunk, b"endobj") {
                        // Include everything before "endobj" but not "endobj" itself
                        data.extend_from_slice(&chunk[..endobj_pos]);
                        break;
                    }
                }

                data.extend_from_slice(&chunk);
            }
        }

        // Parse the object data
        log::debug!(
            "About to parse object {} gen {} ({} bytes)",
            obj_ref.id,
            obj_ref.gen,
            data.len()
        );

        // Corrupted objects degrade to Null so extraction can continue on
        // partial PDFs rather than aborting.
        let mut obj = match parse_object(&data) {
            Ok((_, parsed_obj)) => parsed_obj,
            Err(e) => {
                let error_kind = match &e {
                    nom::Err::Incomplete(_) => "Incomplete data",
                    nom::Err::Error(err) | nom::Err::Failure(err) => match err.code {
                        nom::error::ErrorKind::Eof => "Unexpected EOF",
                        nom::error::ErrorKind::Tag => "Expected tag not found",
                        nom::error::ErrorKind::Fail => "Parse failed",
                        _ => "Parse error",
                    },
                };
                log::warn!(
                    "Object {} at offset {} is corrupted ({}), using Null placeholder. \
                     This may result in missing content from the PDF.",
                    obj_ref.id,
                    offset,
                    error_kind
                );
                Object::Null
            }
        };

        // Decrypt string values inside this uncompressed object before
        // caching. Skip the /Encrypt dict (its entries are key material)
        // and the non-authenticated case (no key derived yet). Strings
        // inside compressed objects ride along with the ObjStm payload
        // and are already in clear text per ISO 32000-1:2008 §7.6.2.
        let is_encrypt_dict = *self.encrypt_dict_ref.lock_or_recover() == Some(obj_ref);
        if !is_encrypt_dict {
            let handler_guard = self.encryption_handler.lock_or_recover();
            if let Some(handler) = handler_guard.as_ref() {
                if handler.is_authenticated() {
                    Self::decrypt_strings_in_object(
                        handler,
                        &mut obj,
                        obj_ref.id,
                        obj_ref.gen as u32,
                    );
                }
            }
        }

        // Cache the object
        self.object_cache
            .lock_or_recover()
            .insert(obj_ref, obj.clone());

        Ok(obj)
    }

    /// Load a compressed object from an object stream (Type 2 xref entry).
    ///
    /// # Arguments
    ///
    /// * `obj_ref` - The object reference being loaded
    /// * `stream_obj_num` - The object number of the object stream
    /// * `index_in_stream` - The index within the stream (unused but provided for completeness)
    fn load_compressed_object(
        &self,
        obj_ref: ObjectRef,
        stream_obj_num: u32,
        _index_in_stream: u16,
    ) -> Result<Object> {
        use crate::objstm::parse_object_stream_with_decryption;

        log::debug!(
            "[load_compressed_debug] Loading obj {} from stream {}",
            obj_ref.id,
            stream_obj_num
        );

        // Per PDF §7.6.3, object streams (/Type /ObjStm) shall NOT be individually
        // encrypted. Encryption initialization is therefore not required to read an
        // ObjStm: the unencrypted parse path below is always attempted first. If
        // initialization fails (e.g. unsupported algorithm, no legacy-crypto feature),
        // log and continue — the handler will be None and we'll use the no-decryption
        // path, which is exactly what the spec mandates for ObjStm content.
        if let Err(e) = self.ensure_encryption_initialized() {
            log::debug!(
                "Encryption init skipped for ObjStm {} load ({}); will parse without decryption",
                stream_obj_num,
                e
            );
        }

        // Load the object stream
        let stream_ref = ObjectRef::new(stream_obj_num, 0);
        let stream_obj = self.load_uncompressed_object(stream_ref, {
            // Look up the stream's offset in the xref table
            let stream_entry = match self.xref.get(stream_obj_num) {
                Some(entry) => entry,
                None => {
                    // PDF Spec §7.3.10: treat as null
                    log::warn!(
                        "Object stream {} not in xref, treating compressed object {} as Null",
                        stream_obj_num,
                        obj_ref.id
                    );
                    self.object_cache
                        .lock_or_recover()
                        .insert(obj_ref, Object::Null);
                    return Ok(Object::Null);
                }
            };

            if stream_entry.entry_type != crate::xref::XRefEntryType::Uncompressed {
                return Err(Error::InvalidPdf(format!(
                    "object stream {} is not an uncompressed object",
                    stream_obj_num
                )));
            }

            stream_entry.offset
        })?;

        // Parse all objects from the stream.
        //
        // Per ISO 32000-2:2020 Section 7.6.3, object streams (/Type /ObjStm)
        // cross-reference streams (/Type /XRef) shall NOT be individually encrypted.
        // The stream data is only compressed, not encrypted. Many PDF producers
        // (including many real-world producers) follow this rule even under
        // PDF 1.x, so attempting AES decryption on the raw stream bytes fails
        // because the data length is not a multiple of the AES block size (16).
        //
        // We therefore always parse object streams WITHOUT decryption. If a
        // future PDF is encountered where the producer DID encrypt the ObjStm
        // (non-standard), the unencrypted parse will fail and we fall back to
        // trying with decryption.
        let handler_ref = self.encryption_handler.lock_or_recover();
        let objects_map = if handler_ref.is_some() {
            // First try without decryption (spec-compliant path)
            match parse_object_stream_with_decryption(&stream_obj, None, 0, 0) {
                Ok(map) => map,
                Err(_no_decrypt_err) => {
                    // Fallback: try with decryption for non-standard producers
                    let handler = handler_ref.as_ref().unwrap();
                    let decrypt_fn = |data: &[u8]| -> Result<Vec<u8>> {
                        handler.decrypt_stream(data, stream_obj_num, 0)
                    };
                    parse_object_stream_with_decryption(
                        &stream_obj,
                        Some(&decrypt_fn),
                        stream_obj_num,
                        0,
                    )?
                }
            }
        } else {
            parse_object_stream_with_decryption(&stream_obj, None, 0, 0)?
        };
        drop(handler_ref);

        // Extract the requested object
        let obj = match objects_map.get(&obj_ref.id) {
            Some(o) => o.clone(),
            None => {
                // PDF Spec §7.3.10: treat as null
                log::warn!(
                    "Object {} not found in object stream {}, treating as Null",
                    obj_ref.id,
                    stream_obj_num
                );
                Object::Null
            }
        };

        // Cache all objects from the stream for future access
        // IMPORTANT: Only cache objects whose xref entry points to THIS stream.
        // In incremental updates, the same object number may exist in multiple streams,
        // and we must not cache a stale version from an older stream.
        for (obj_num, object) in objects_map {
            let cache_ref = ObjectRef::new(obj_num, 0);
            let should_cache = if let Some(entry) = self.xref.get(obj_num) {
                // Only cache if the xref says this object belongs to this stream
                entry.entry_type == crate::xref::XRefEntryType::Compressed
                    && entry.offset == stream_obj_num as u64
            } else {
                // Object not in xref at all -- safe to cache as it's only in this stream
                true
            };
            if should_cache {
                self.object_cache
                    .lock_or_recover()
                    .insert(cache_ref, object);
            } else {
                log::debug!(
                    "[cache_debug] NOT caching obj {} from stream {} (xref points elsewhere)",
                    obj_num,
                    stream_obj_num
                );
            }
        }

        Ok(obj)
    }

    /// Find object header by searching backwards from a given offset.
    ///
    /// Some PDF generators create xref tables with incorrect offsets that point
    /// to the object body instead of the header. This function searches backwards
    /// from the xref offset to find the actual "N G obj" header.
    ///
    /// We search up to 100 bytes backwards, looking for a line that matches
    /// the expected object header format.
    fn find_object_header_backwards(&self, obj_ref: ObjectRef, wrong_offset: u64) -> Result<u64> {
        // Don't search before the start of the file
        if wrong_offset == 0 {
            return Err(Error::ParseError {
                offset: wrong_offset as usize,
                reason: "Cannot search backwards from offset 0".to_string(),
            });
        }

        // Search up to 100 bytes backwards (reasonable for most PDFs)
        let search_distance = std::cmp::min(100, wrong_offset);
        let search_start = wrong_offset - search_distance;

        // Read the search region under one lock guard (#398 Race A).
        let mut buffer = vec![0u8; search_distance as usize + 100]; // Extra bytes to read full line
        let bytes_read = {
            let mut reader = self.reader.lock_or_recover();
            reader.seek(SeekFrom::Start(search_start))?;
            reader.read(&mut buffer)?
        };

        if bytes_read == 0 {
            return Err(Error::ParseError {
                offset: wrong_offset as usize,
                reason: "Could not read backwards search region".to_string(),
            });
        }

        // Build the expected header pattern as bytes (NOT string to avoid UTF-8 corruption)
        let expected_header = format!("{} {} obj", obj_ref.id, obj_ref.gen);
        let pattern_bytes = expected_header.as_bytes();

        // Search for the byte pattern directly (avoids UTF-8 conversion issues with binary data)
        // Find the match closest to wrong_offset (prefer before, but allow small offsets after)
        let mut best_match: Option<(usize, i64)> = None; // (position, distance_from_wrong)

        for (i, window) in buffer[..bytes_read]
            .windows(pattern_bytes.len())
            .enumerate()
        {
            if window == pattern_bytes {
                let candidate_offset = search_start + i as u64;
                let distance = (candidate_offset as i64) - (wrong_offset as i64);

                // Accept matches within -100 to +10 bytes of wrong_offset
                // (xref might be slightly off by a few bytes)
                if (-100..=10).contains(&distance) {
                    // Prefer the match closest to wrong_offset
                    let is_better = best_match
                        .as_ref()
                        .is_none_or(|(_, best_dist)| distance.abs() < best_dist.abs());

                    if is_better {
                        best_match = Some((i, distance));
                    }
                }
            }
        }

        if let Some((pos, distance)) = best_match {
            let absolute_offset = search_start + pos as u64;
            log::debug!(
                "Found object header '{}' at offset {} ({:+} bytes from xref at {})",
                expected_header,
                absolute_offset,
                distance,
                wrong_offset
            );
            return Ok(absolute_offset);
        }

        // Try with whitespace variations (space, double-space, tab between obj_id and gen)
        let patterns = [
            format!("{} {} obj", obj_ref.id, obj_ref.gen).into_bytes(),
            format!("{}  {} obj", obj_ref.id, obj_ref.gen).into_bytes(),
            format!("{}\t{} obj", obj_ref.id, obj_ref.gen).into_bytes(),
            format!("{} {}\tobj", obj_ref.id, obj_ref.gen).into_bytes(),
        ];

        for pattern in &patterns {
            let mut best_match: Option<(usize, i64)> = None;

            for (i, window) in buffer[..bytes_read].windows(pattern.len()).enumerate() {
                if window == pattern.as_slice() {
                    let candidate_offset = search_start + i as u64;
                    let distance = (candidate_offset as i64) - (wrong_offset as i64);

                    if (-100..=10).contains(&distance) {
                        let is_better = best_match
                            .as_ref()
                            .is_none_or(|(_, best_dist)| distance.abs() < best_dist.abs());

                        if is_better {
                            best_match = Some((i, distance));
                        }
                    }
                }
            }

            if let Some((pos, distance)) = best_match {
                let absolute_offset = search_start + pos as u64;
                log::debug!(
                    "Found object header '{}' at offset {} ({:+} bytes, pattern match)",
                    expected_header,
                    absolute_offset,
                    distance
                );
                return Ok(absolute_offset);
            }
        }

        Err(Error::ParseError {
            offset: wrong_offset as usize,
            reason: format!(
                "Could not find object header '{}' within {} bytes before offset",
                expected_header, search_distance
            ),
        })
    }

    /// Get the document catalog (root object).
    ///
    /// The catalog is the root of the document's object hierarchy.
    /// It contains references to the page tree, outlines, etc.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The /Root entry is present but is not a reference
    /// - Loading the catalog object fails
    /// - The trailer omits /Root **and** no `/Type /Catalog` object can be
    ///   found by scanning (the issue #509 recovery path: a missing /Root is
    ///   not itself fatal — the Catalog is discovered by object scan, as
    ///   Poppler / PDFium do — but it does error if that scan also fails)
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let catalog = doc.catalog()?;
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn catalog(&self) -> Result<Object> {
        let trailer_dict = self
            .trailer
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf("Trailer is not a dictionary".to_string()))?;

        if let Some(root_obj) = trailer_dict.get("Root") {
            let root_ref = root_obj
                .as_reference()
                .ok_or_else(|| Error::InvalidPdf("/Root is not a reference".to_string()))?;
            return self.load_object(root_ref);
        }

        // The trailer omits /Root. A Linearized file's sparse end-of-file
        // trailer legitimately does this; discover the Catalog
        // by scanning indirect objects for /Type /Catalog, as Poppler /
        // PDFium do.
        self.find_catalog_by_scan().ok_or_else(|| {
            Error::InvalidPdf(
                "Trailer omits /Root and no /Type /Catalog object could be found by scanning"
                    .to_string(),
            )
        })
    }

    /// Scan indirect objects for the document Catalog (`/Type /Catalog`).
    ///
    /// Used only as a fallback when the trailer omits `/Root`.
    /// Bounded so a pathological xref can't turn this into an unbounded
    /// scan; the Catalog is virtually always one of the first objects.
    ///
    /// The smallest `MAX_SCAN` object numbers are scanned, ascending.
    /// `all_object_numbers()` is `HashMap`-backed, so iterating it directly
    /// would be nondeterministic — a bounded scan over an arbitrary subset
    /// can miss the Catalog on different runs. `smallest_object_numbers`
    /// makes discovery deterministic, scans low-numbered objects first
    /// (where the Catalog conventionally lives), and bounds the candidate
    /// set *before* sorting so a pathological xref stays O(n log MAX_SCAN).
    fn find_catalog_by_scan(&self) -> Option<Object> {
        const MAX_SCAN: usize = 4096;
        let nums = self.xref.smallest_object_numbers(MAX_SCAN);
        let mut checked = 0usize;
        for num in nums {
            if checked >= MAX_SCAN {
                break;
            }
            let generation = match self.xref.get(num) {
                Some(e) if e.in_use => e.generation,
                _ => continue,
            };
            checked += 1;
            if let Ok(obj) = self.load_object(ObjectRef::new(num, generation)) {
                if obj
                    .as_dict()
                    .and_then(|d| d.get("Type"))
                    .and_then(|t| t.as_name())
                    == Some("Catalog")
                {
                    log::info!(
                        "Catalog discovered by object scan: {} {} obj",
                        num,
                        generation
                    );
                    return Some(obj);
                }
            }
        }
        None
    }

    /// Get the structure tree (logical structure) of the document.
    ///
    /// Tagged PDFs contain a structure tree that defines the logical structure
    /// and reading order of the document. This is the PDF-spec-compliant way
    /// to determine reading order.
    ///
    /// Returns `Ok(Some(StructTreeRoot))` if the document has a structure tree,
    /// `Ok(None)` if it's not a tagged PDF, or an error if parsing fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// if let Some(struct_tree) = doc.structure_tree()? {
    ///     println!("This is a Tagged PDF with logical structure");
    /// } else {
    ///     println!("This PDF does not have a structure tree");
    /// }
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn structure_tree(&self) -> Result<Option<crate::structure::StructTreeRoot>> {
        crate::structure::parse_structure_tree(self)
    }

    /// Returns the document's structure tree, bounding the parse work by an
    /// optional wall-clock `budget`.
    ///
    /// `None` parses the complete tree (identical to [`Self::structure_tree`]);
    /// `Some(duration)` returns `Ok(None)` if parsing exceeds that budget, so a
    /// latency-sensitive caller can fall back to another strategy. Prefer `None`
    /// unless you have a concrete responsiveness requirement.
    pub fn structure_tree_with_budget(
        &self,
        budget: Option<std::time::Duration>,
    ) -> Result<Option<crate::structure::StructTreeRoot>> {
        crate::structure::parse_structure_tree_with_budget(self, budget)
    }

    /// Returns the document's structure tree **only when it is trustworthy for
    /// reading-order purposes**, per ISO 32000-1:2008 §14.8.2.3.1 and §14.7.1.
    ///
    /// A `/StructTreeRoot` encodes the producer's *logical structure order* — a
    /// depth-first traversal of the tag hierarchy — which is authoritative for
    /// reading order independent of glyph geometry (§14.7.1). It is trusted when
    /// the document is `/Marked` (Tagged PDF) **or** the catalog directly
    /// references a `/StructTreeRoot` (PDF 1.3/1.4 tagged files predate the
    /// `/MarkInfo` dictionary; §7.7.2) — matching the historical gate so output
    /// for non-suspect documents is byte-for-byte unchanged — **and**
    /// `/MarkInfo /Suspects` is not `true`. A `true` `/Suspects` flag is the
    /// spec-sanctioned signal (the `/TagSuspect /Ordering` mechanism,
    /// §14.8.2.3.1) that page content order may not match logical structure
    /// order, so the tree is rejected and callers fall back to geometric order.
    ///
    /// Shares `structure_tree_cache`, so this costs a single cached parse.
    pub(crate) fn struct_tree_trustworthy(&self) -> Option<Arc<crate::structure::StructTreeRoot>> {
        let mark = self.mark_info().unwrap_or_default();
        // Suspect documents: geometric reading order is spec-correct
        // (§14.8.2.3.1). This is the only behavioural change versus the legacy
        // inline gate, which never consulted /Suspects.
        if mark.suspects {
            return None;
        }
        let cached = self.structure_tree_cache.lock_or_recover().clone();
        match cached {
            Some(tree) => tree,
            None => {
                let has_struct_tree_root = self
                    .catalog()
                    .ok()
                    .and_then(|cat| cat.as_dict().map(|d| d.contains_key("StructTreeRoot")))
                    .unwrap_or(false);
                let tree = if mark.marked || has_struct_tree_root {
                    self.structure_tree().ok().flatten().map(Arc::new)
                } else {
                    None
                };
                *self.structure_tree_cache.lock_or_recover() = Some(tree.clone());
                tree
            }
        }
    }

    /// Returns the document's structure tree whenever it is **available**,
    /// independent of `/MarkInfo /Suspects`.
    ///
    /// The `/Suspects` flag (§14.7.1) signals that the producer's *reading
    /// order* may be unreliable, so `struct_tree_trustworthy` rejects the
    /// tree for ordering. `/ActualText`, however, is content replacement
    /// (§14.9.4) and remains trustworthy: a producer that bothered to
    /// supply the replacement text for a glyph run is asserting what
    /// that run is *meant* to read as, regardless of whether sibling
    /// reading-order tags are reliable. This accessor lets the
    /// ActualText pipeline honour the producer's intent on Suspects=true
    /// documents while geometric reading order takes over the ordering
    /// problem.
    ///
    /// Shares `structure_tree_cache` with `struct_tree_trustworthy`, so
    /// both predicates cost a single cached parse.
    pub(crate) fn struct_tree_marked(&self) -> Option<Arc<crate::structure::StructTreeRoot>> {
        let cached = self.structure_tree_cache.lock_or_recover().clone();
        match cached {
            Some(tree) => tree,
            None => {
                let mark = self.mark_info().unwrap_or_default();
                let has_struct_tree_root = self
                    .catalog()
                    .ok()
                    .and_then(|cat| cat.as_dict().map(|d| d.contains_key("StructTreeRoot")))
                    .unwrap_or(false);
                let tree = if mark.marked || has_struct_tree_root {
                    self.structure_tree().ok().flatten().map(Arc::new)
                } else {
                    None
                };
                *self.structure_tree_cache.lock_or_recover() = Some(tree.clone());
                tree
            }
        }
    }

    /// Returns the cached [`ActualTextIndex`] for this document.
    ///
    /// Builds the index lazily on first call, then serves cached copies.
    /// Returns `None` for untagged documents and for tagged documents
    /// whose structure tree carries no `/ActualText`.
    ///
    /// Decoupled from `/MarkInfo /Suspects` — see [`struct_tree_marked`].
    pub(crate) fn actualtext_index(&self) -> Option<Arc<crate::structure::ActualTextIndex>> {
        if let Some(cached) = self.actualtext_index_cache.lock_or_recover().clone() {
            return cached;
        }
        let tree = self.struct_tree_marked();
        let built = tree.and_then(|t| {
            let idx = crate::structure::traversal::build_actualtext_index(&t);
            if idx.is_empty() {
                None
            } else {
                Some(Arc::new(idx))
            }
        });
        *self.actualtext_index_cache.lock_or_recover() = Some(built.clone());
        built
    }

    /// Whether text extraction uses the Tagged-PDF *logical structure order* (a
    /// depth-first traversal of `/StructTreeRoot`) rather than geometric
    /// page-content order for this document.
    ///
    /// Returns `true` exactly when the document carries a trustworthy structure
    /// tree per ISO 32000-1:2008 §14.8.2.3.1 / §14.7.1: it is `/Marked` or the
    /// catalog references a `/StructTreeRoot`, the tree resolves non-empty, and
    /// `/MarkInfo /Suspects` is not `true`. When `false`, extraction falls back
    /// to geometric reading order. This is a read-only introspection accessor;
    /// it does not change extraction behaviour.
    pub fn prefers_structure_reading_order(&self) -> bool {
        self.struct_tree_trustworthy().is_some()
    }

    /// Find the document's default CMYK output-intent profile.
    ///
    /// Per ISO 32000-1:2008 §14.11.5, an `/OutputIntents` array in the
    /// catalog advertises the colour characteristics of the target
    /// output device. Each entry is a dictionary; the `DestOutputProfile`
    /// key (when present) references an ICC profile stream identifying
    /// the intended press / display calibration.
    ///
    /// This method returns the **first CMYK** `DestOutputProfile` it
    /// finds (N = 4) — the usual match for "here is how my CMYK ink
    /// should look" on PDF/X files. Callers can use it as a fallback
    /// profile for plain `/DeviceCMYK` images that lack their own ICC
    /// colour space.
    ///
    /// Returns `None` when no output intent exists, no CMYK entry is
    /// present, or the profile stream can't be parsed as ICC.
    pub fn output_intent_cmyk_profile(&self) -> Option<std::sync::Arc<crate::color::IccProfile>> {
        // Memoise the (potentially expensive) decode + parse: hot rendering
        // paths consult this accessor once per paint, and qcms / lcms2
        // header validation + LUT decode on a hundreds-of-KB profile is
        // not free. `Some(None)` means "checked once, no usable CMYK
        // OutputIntent"; a subsequent call must NOT re-walk the catalog.
        if let Some(cached) = self
            .output_intent_cmyk_profile_cache
            .lock_or_recover()
            .as_ref()
        {
            return cached.clone();
        }
        let resolved = self.compute_output_intent_cmyk_profile();
        *self.output_intent_cmyk_profile_cache.lock_or_recover() = Some(resolved.clone());
        resolved
    }

    /// True when the document catalog declares an `/OutputIntents`
    /// array, regardless of whether the contained profile bytes
    /// successfully parse. Coupled with
    /// [`Self::output_intent_cmyk_profile`] returning `None`, this
    /// distinguishes "no OutputIntent requested" (acceptable silent
    /// fallback) from "OutputIntent requested but unusable" (degraded
    /// press output that callers should warn about). Tracks upstream
    /// issue yfedoseev/pdf_oxide#712 on swallowed profile-parse
    /// diagnostics.
    pub fn has_output_intents_declaration(&self) -> bool {
        let Ok(catalog) = self.catalog() else {
            return false;
        };
        let Some(cat_dict) = catalog.as_dict() else {
            return false;
        };
        let Some(intents_obj) = cat_dict.get("OutputIntents") else {
            return false;
        };
        let intents_obj = match intents_obj {
            Object::Reference(r) => match self.load_object(*r) {
                Ok(o) => o,
                Err(_) => return false,
            },
            other => other.clone(),
        };
        matches!(intents_obj, Object::Array(_))
    }

    fn compute_output_intent_cmyk_profile(
        &self,
    ) -> Option<std::sync::Arc<crate::color::IccProfile>> {
        let catalog = self.catalog().ok()?;
        let cat_dict = catalog.as_dict()?;

        let intents_obj = cat_dict.get("OutputIntents")?;
        let intents_obj = match intents_obj {
            Object::Reference(r) => self.load_object(*r).ok()?,
            _ => intents_obj.clone(),
        };
        let intents_arr = match &intents_obj {
            Object::Array(a) => a.clone(),
            _ => return None,
        };

        for entry in intents_arr {
            let entry = match entry {
                // Skip a broken entry rather than aborting the whole array (§7.3.10).
                Object::Reference(r) => match self.load_object(r) {
                    Ok(o) => o,
                    Err(e) => {
                        log::warn!("OutputIntents entry {r} could not be loaded ({e:?}); skipping");
                        continue;
                    }
                },
                other => other,
            };
            let entry_dict = match entry.as_dict() {
                Some(d) => d.clone(),
                None => continue,
            };
            let profile_obj = match entry_dict.get("DestOutputProfile") {
                Some(p) => p.clone(),
                None => continue,
            };
            let profile_label = match &profile_obj {
                Object::Reference(r) => format!("DestOutputProfile {r}"),
                _ => "inline DestOutputProfile".to_string(),
            };
            let profile_stream = match profile_obj {
                Object::Reference(r) => {
                    match self.load_object(r) {
                        Ok(o) => o,
                        Err(e) => {
                            log::warn!("OutputIntent {profile_label} could not be loaded ({e:?}); skipping");
                            continue;
                        }
                    }
                }
                other => other,
            };

            let Object::Stream { dict, .. } = &profile_stream else {
                continue;
            };
            let n = match dict.get("N").and_then(|o| o.as_integer()) {
                Some(4) => 4u8, // only CMYK; ignore RGB/Gray output intents here
                _ => continue,
            };
            let bytes = match profile_stream.decode_stream_data() {
                Ok(b) => b,
                Err(e) => {
                    log::warn!(
                        "OutputIntent {profile_label} stream failed to decode ({e:?}); skipping"
                    );
                    continue;
                }
            };
            match crate::color::IccProfile::parse(bytes, n) {
                Some(prof) => return Some(std::sync::Arc::new(prof)),
                None => {
                    log::warn!(
                        "OutputIntent {profile_label} is not a valid N=4 ICC profile; skipping"
                    )
                }
            }
        }
        None
    }

    /// Get the MarkInfo dictionary from the document catalog.
    ///
    /// The MarkInfo dictionary indicates whether the document conforms to
    /// Tagged PDF conventions and whether the structure tree might contain
    /// suspect (unreliable) content.
    ///
    /// Per ISO 32000-1:2008 Section 14.7.1, the MarkInfo dictionary contains:
    /// - `/Marked` - Whether the document conforms to Tagged PDF conventions
    /// - `/Suspects` - Whether the document contains suspect content
    /// - `/UserProperties` - Whether the document uses user properties
    ///
    /// # Returns
    ///
    /// Returns `MarkInfo` with the parsed values, or default values if
    /// the MarkInfo dictionary is not present.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let mark_info = doc.mark_info()?;
    /// if mark_info.is_structure_reliable() {
    ///     println!("Structure tree can be trusted for reading order");
    /// } else if mark_info.suspects {
    ///     println!("Structure tree may contain unreliable content");
    /// }
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn mark_info(&self) -> Result<crate::structure::MarkInfo> {
        let catalog = self.catalog()?;
        let catalog_dict = match catalog.as_dict() {
            Some(d) => d,
            None => return Ok(crate::structure::MarkInfo::default()),
        };

        // Get /MarkInfo dictionary
        let mark_info_obj = match catalog_dict.get("MarkInfo") {
            Some(obj) => obj,
            None => return Ok(crate::structure::MarkInfo::default()),
        };

        // Resolve reference if needed
        let mark_info_obj = if let Some(r) = mark_info_obj.as_reference() {
            self.load_object(r)?
        } else {
            mark_info_obj.clone()
        };

        let mark_info_dict = match mark_info_obj.as_dict() {
            Some(d) => d,
            None => return Ok(crate::structure::MarkInfo::default()),
        };

        // Parse boolean fields with defaults of false
        let marked = mark_info_dict
            .get("Marked")
            .and_then(|o: &crate::object::Object| o.as_bool())
            .unwrap_or(false);

        let suspects = mark_info_dict
            .get("Suspects")
            .and_then(|o: &crate::object::Object| o.as_bool())
            .unwrap_or(false);

        let user_properties = mark_info_dict
            .get("UserProperties")
            .and_then(|o: &crate::object::Object| o.as_bool())
            .unwrap_or(false);

        Ok(crate::structure::MarkInfo {
            marked,
            suspects,
            user_properties,
        })
    }

    /// Get the number of pages in the document.
    ///
    /// This function:
    /// 1. Loads the catalog (root object)
    /// 2. Follows the /Pages reference to the page tree root
    /// 3. Extracts the /Count value from the page tree
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The catalog cannot be loaded
    /// - The /Pages entry is missing or invalid
    /// - The page tree root does not contain a /Count entry
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let count = doc.page_count()?;
    /// println!("Document has {} pages", count);
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn page_count(&self) -> Result<usize> {
        // Standard /Count reader, then a manual page-tree scan on failure.
        let primary: Result<usize> = match self.get_page_count_standard() {
            Ok(count) => {
                log::debug!("Page count from /Count: {}", count);
                Ok(count)
            }
            Err(Error::EncryptedPdf) => return Err(Error::EncryptedPdf),
            Err(e) => {
                // For encrypted PDFs any failure to read the page tree means we
                // cannot access the content, so surface it immediately.
                if self.is_encrypted() {
                    log::warn!("Page count failed for encrypted PDF: {}", e);
                    return Err(Error::EncryptedPdf);
                }
                log::warn!("Failed to get page count from /Count: {}", e);
                log::info!("Falling back to scanning page tree");
                match self.get_page_count_by_scanning() {
                    Ok(count) => {
                        log::info!("Page count from scanning: {}", count);
                        Ok(count)
                    }
                    Err(scan_err) => {
                        log::error!("Both methods failed. Standard: {}, Scan: {}", e, scan_err);
                        Err(e) // Return original error
                    }
                }
            }
        };

        // Enumerator rescue. A count of 0 from the /Count-based readers on a
        // non-encrypted document is almost always a page tree they could not
        // resolve - `/Pages` packed inside an object stream, or a deeply nested
        // `/Pages` -> `/Pages` -> `/Page` tree - not a genuinely empty document.
        // The /Count readers and `all_page_refs` (which walks `/Pages` -> `/Kids`
        // via `collect_page_refs`) both MISS such a tree; `get_page` still reaches
        // every page through its own per-page traversal / `collect_all_pages` bulk
        // walk, so count by agreeing with what it can actually reach. Gated on a
        // primary result of 0, so every document the standard reader counts
        // normally is unchanged.
        if matches!(primary, Ok(0)) && !self.is_encrypted() {
            // The /Count readers - and `all_page_refs`, which walks
            // `/Pages` -> `/Kids` via `collect_page_refs` - miss a page tree
            // packed inside an object stream. `get_page` still resolves every
            // such page through its own per-page traversal / `collect_all_pages`
            // bulk walk, so count by probing it: the definitive agreement with
            // the pages the rest of the API can actually reach. `get_page` never
            // calls back into `page_count` (no recursion) and caches each page
            // (repeat probes are cheap). For an ObjStm-packed tree each `get_page`
            // can fall back to a full object scan, so counting this way is
            // O(n * objects) - bounded by the sanity cap, and only ever on an
            // already-broken document. Only runs when the primary count is 0, so
            // normally-counted documents are byte-identical.
            let mut n = 0usize;
            while n < 1_000_000 && self.get_page(n).is_ok() {
                n += 1;
            }
            if n > 0 {
                log::info!("Page /Count was 0; enumerated {} pages via get_page", n);
                return Ok(n);
            }
        }
        primary
    }

    /// Get the MediaBox of a page (v0.3.14).
    ///
    /// MediaBox defines the physical boundaries of the page in user space units.
    pub fn get_page_media_box(&self, page_index: usize) -> Result<(f32, f32, f32, f32)> {
        let page = self.get_page(page_index)?;
        let page_dict = page
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf("Page is not a dictionary".to_string()))?;

        // Resolve indirect reference if present — PDF spec §7.3.10 permits any value
        // to be an indirect reference, e.g. `/MediaBox 174 0 R` where 174 0 R is `[0 0 612 792]`.
        let media_box_obj_raw = page_dict
            .get("MediaBox")
            .ok_or_else(|| Error::InvalidPdf("MediaBox not found or not an array".to_string()))?;
        let media_box_obj = self.resolve_obj_ref(media_box_obj_raw);
        let media_box = media_box_obj
            .as_array()
            .ok_or_else(|| Error::InvalidPdf("MediaBox not found or not an array".to_string()))?;

        if media_box.len() < 4 {
            return Err(Error::InvalidPdf(
                "MediaBox must have at least 4 elements".to_string(),
            ));
        }

        fn to_f32(obj: &Object) -> f32 {
            match obj {
                Object::Integer(v) => *v as f32,
                Object::Real(v) => *v as f32,
                _ => 0.0,
            }
        }

        // §7.3.10: *any* element of the rectangle array may itself be an
        // indirect reference (pdf.js issue7872 stores `/MediaBox
        // [4 0 R 5 0 R 6 0 R 7 0 R]`). Resolve each element before
        // coercing — otherwise an unresolved Reference reads as 0.0 and
        // the page collapses to a zero-area box that clips all content.
        Ok((
            to_f32(&self.resolve_obj_ref(&media_box[0])),
            to_f32(&self.resolve_obj_ref(&media_box[1])),
            to_f32(&self.resolve_obj_ref(&media_box[2])),
            to_f32(&self.resolve_obj_ref(&media_box[3])),
        ))
    }

    /// Page `/Rotate` normalised to one of `{0, 90, 180, 270}`
    /// (ISO 32000-1 §7.7.3.3); `0` when absent or invalid.
    ///
    /// Pure inspection (no feature gate) for the auto-extraction
    /// classifier (#517 case I — transformed-bbox coverage / OCR
    /// orientation). Resolves via [`get_page`](Self::get_page), so the
    /// inheritable `/Rotate` attribute (ISO 32000-1 §7.7.3.4) is walked
    /// up the page tree — a `/Rotate` set on an ancestor `/Pages` node
    /// is honoured, not just one on the leaf page object.
    pub fn get_page_rotation(&self, page_index: usize) -> Result<i32> {
        let page = self.get_page(page_index)?;
        let dict = page
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf("Page is not a dictionary".to_string()))?;
        let raw = match dict.get("Rotate") {
            Some(r) => match self.resolve_obj_ref(r) {
                Object::Integer(v) => v as i32,
                Object::Real(v) => v as i32,
                _ => 0,
            },
            None => 0,
        };
        // `/Rotate` shall be a multiple of 90 (ISO 32000-1 §7.7.3.3);
        // a non-multiple is invalid → `0` (per this fn's contract),
        // NOT silently floored (e.g. 135 must not become 90).
        let n = ((raw % 360) + 360) % 360;
        Ok(if n % 90 == 0 { n } else { 0 })
    }

    /// Get page count using the standard /Count field
    fn get_page_count_standard(&self) -> Result<usize> {
        // Load catalog
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;

        // Get /Pages reference
        let pages_ref = catalog_dict
            .get("Pages")
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?
            .as_reference()
            .ok_or_else(|| Error::InvalidPdf("/Pages is not a reference".to_string()))?;

        // Load page tree root
        let pages_obj = self.load_object(pages_ref)?;
        let pages_dict = match pages_obj.as_dict() {
            Some(d) => d,
            None => {
                // If the page tree root resolved to Null it usually means the
                // PDF is encrypted and the page tree could not be decrypted.
                // Surface the real error instead of silently reporting 0 pages.
                if matches!(pages_obj, crate::object::Object::Null) && self.is_encrypted() {
                    return Err(Error::EncryptedPdf);
                }
                log::warn!(
                    "Page tree root is {} (expected Dictionary), treating as 0 pages",
                    pages_obj.type_name()
                );
                return Ok(0);
            }
        };

        // Get /Count
        let count = pages_dict
            .get("Count")
            .ok_or_else(|| Error::InvalidPdf("Page tree missing /Count entry".to_string()))?
            .as_integer()
            .ok_or_else(|| Error::InvalidPdf("/Count is not an integer".to_string()))?;

        // Validate /Count against PDF spec limits (Annex C.2: max 8,388,607 indirect objects)
        const MAX_PAGES: i64 = 8_388_607;
        if !(0..=MAX_PAGES).contains(&count) {
            log::warn!(
                "/Count value {} is unreasonable (max {}), falling back to tree scan",
                count,
                MAX_PAGES
            );
            return self.get_page_count_by_scanning();
        }

        // Sanity check: /Count can't exceed total objects in the file
        let max_objects = self.xref.len();
        if (count as usize) > max_objects {
            log::warn!(
                "/Count {} exceeds total objects {}, falling back to tree scan",
                count,
                max_objects
            );
            return self.get_page_count_by_scanning();
        }

        Ok(count as usize)
    }

    /// Get page count by scanning the page tree (fallback method)
    fn get_page_count_by_scanning(&self) -> Result<usize> {
        // Load catalog
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;

        // Get /Pages reference
        let pages_ref = catalog_dict
            .get("Pages")
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?
            .as_reference()
            .ok_or_else(|| Error::InvalidPdf("/Pages is not a reference".to_string()))?;

        // Count pages by traversing the tree
        self.count_pages_recursive(pages_ref, 0)
    }

    /// Recursively count pages in the page tree
    fn count_pages_recursive(&self, node_ref: ObjectRef, depth: usize) -> Result<usize> {
        // Prevent infinite recursion
        const MAX_DEPTH: usize = 50;
        if depth > MAX_DEPTH {
            log::warn!("Page tree depth exceeded {} levels, stopping", MAX_DEPTH);
            return Ok(0);
        }

        // Load the node
        let node = match self.load_object(node_ref) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("Failed to load page tree node {}: {}", node_ref, e);
                return Ok(0); // Skip this node
            }
        };

        let node_dict = match node.as_dict() {
            Some(d) => d,
            None => {
                log::warn!("Page tree node {} is not a dictionary", node_ref);
                return Ok(0);
            }
        };

        // Check node type
        let node_type = node_dict.get("Type").and_then(|obj| obj.as_name());

        match node_type {
            Some("Page") => {
                // This is a leaf page
                Ok(1)
            }
            Some("Pages") => {
                // This is an intermediate node with kids
                let kids = match node_dict.get("Kids").and_then(|obj| obj.as_array()) {
                    Some(k) => k,
                    None => {
                        log::warn!("Pages node {} missing /Kids array", node_ref);
                        return Ok(0);
                    }
                };

                let mut count = 0;
                for kid in kids {
                    if let Some(kid_ref) = kid.as_reference() {
                        match self.count_pages_recursive(kid_ref, depth + 1) {
                            Ok(page_count) => count += page_count,
                            Err(Error::CircularReference(obj_ref)) => {
                                log::warn!(
                                    "Circular reference in page tree at object {}, skipping",
                                    obj_ref
                                );
                                continue;
                            }
                            Err(Error::RecursionLimitExceeded(_)) => {
                                log::warn!(
                                    "Recursion limit exceeded in page tree, skipping branch"
                                );
                                continue;
                            }
                            Err(e) => {
                                log::warn!("Error counting pages in branch: {}, skipping", e);
                                continue;
                            }
                        }
                    }
                }
                Ok(count)
            }
            _ => {
                log::warn!(
                    "Unknown page tree node type: {:?}",
                    node_type.unwrap_or("(none)")
                );
                Ok(0)
            }
        }
    }

    /// Get page count as u32 (legacy API).
    ///
    /// This is a convenience method that returns the page count as a u32.
    /// It calls `page_count()` internally but converts the result
    /// returns 0 if an error occurs (for backward compatibility).
    #[deprecated(
        since = "0.1.0",
        note = "Use page_count() instead, which returns Result"
    )]
    pub fn page_count_u32(&self) -> u32 {
        self.page_count().unwrap_or(0) as u32
    }

    /// Returns the page index range `0..page_count`, or an empty range
    /// when `page_count()` fails. Issue #447.
    ///
    /// Designed for `for i in doc.page_indices() { ... }` so callers
    /// don't have to write `for i in 0..doc.page_count()?`. The
    /// fallible-vs-iterator tension that motivated the issue is
    /// resolved by treating a metadata-broken document as having no
    /// pages at the iteration level — every per-page extraction call
    /// is already fallible and surfaces the real error.
    ///
    /// # Example
    ///
    /// ```ignore
    /// for i in doc.page_indices() {
    ///     let text = doc.extract_text(i)?;
    ///     println!("page {}: {} chars", i, text.len());
    /// }
    /// ```
    pub fn page_indices(&self) -> std::ops::Range<usize> {
        0..self.page_count().unwrap_or(0)
    }

    /// Get a page object by index (0-based).
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Returns
    ///
    /// The page dictionary object.
    ///
    /// # Errors
    ///
    /// Returns an error if the page index is out of bounds or if the page
    /// tree structure is invalid.
    pub fn get_page(&self, page_index: usize) -> Result<Object> {
        // Check page cache first — page tree is static per §7.7.3.2
        if let Some(cached) = self.page_cache.lock_or_recover().get(&page_index).cloned() {
            return Ok(cached);
        }

        // Defer bulk page tree walk until enough pages are accessed.
        const LAZY_THRESHOLD: usize = 64;
        let cache_misses = self.page_cache.lock_or_recover().len();

        if !self.page_cache_populated.load(Ordering::Acquire) && cache_misses >= LAZY_THRESHOLD {
            self.page_cache_populated.store(true, Ordering::Release);
            if let Err(e) = self.populate_page_cache() {
                log::warn!(
                    "Bulk page tree walk failed ({}), falling back to per-page traversal",
                    e
                );
            }
            // Check cache after bulk population
            if let Some(cached) = self.page_cache.lock_or_recover().get(&page_index).cloned() {
                return Ok(cached);
            }
        }

        // Per-page tree traversal: walks only the branches needed to find target page
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;

        let pages_ref = catalog_dict
            .get("Pages")
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?
            .as_reference()
            .ok_or_else(|| Error::InvalidPdf("/Pages is not a reference".to_string()))?;

        let mut inherited = HashMap::new();

        let page = match self.get_page_from_tree(pages_ref, page_index, &mut 0, &mut inherited) {
            Ok(page) => {
                if let Some(dict) = page.as_dict() {
                    log::debug!("Collected page {}, keys: {:?}", page_index, dict.keys());
                    if let Some(contents) = dict.get("Contents") {
                        log::debug!("  -> /Contents: {:?}", contents);
                    }
                    if let Some(rotate) = dict.get("Rotate") {
                        log::debug!("  -> /Rotate: {:?}", rotate);
                    }
                }
                Ok(page)
            }
            Err(e) => {
                if matches!(
                    e,
                    Error::InvalidPdf(_)
                        | Error::InvalidObjectType { .. }
                        | Error::CircularReference(_)
                        | Error::ObjectNotFound(_, _)
                ) {
                    log::warn!(
                        "Page tree traversal failed ({}), trying fallback scan method",
                        e
                    );
                    self.get_page_by_scanning(page_index)
                } else {
                    Err(e)
                }
            }
        }?;

        self.page_cache
            .lock_or_recover()
            .insert(page_index, page.clone());
        Ok(page)
    }

    /// Walk the page tree once and populate page_cache for ALL pages.
    /// This avoids O(n²) cost when pages are accessed sequentially.
    fn populate_page_cache(&self) -> Result<()> {
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;

        let pages_ref = catalog_dict
            .get("Pages")
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?
            .as_reference()
            .ok_or_else(|| Error::InvalidPdf("/Pages is not a reference".to_string()))?;

        let mut page_index = 0usize;
        let mut inherited = HashMap::new();
        self.collect_all_pages(
            pages_ref,
            &mut page_index,
            &mut inherited,
            &mut HashSet::new(),
        )?;
        log::debug!("Populated page cache with {} pages", page_index);
        Ok(())
    }

    /// Pre-populate `image_xobject_cache` for all XObject refs across all cached pages.
    /// Collects all unique XObject references, sorts them by xref offset for sequential
    /// I/O (avoids random seeking in large files), then peeks each one via `is_form_xobject()`.
    #[allow(dead_code)]
    fn prefetch_xobject_subtypes(&self) {
        // Collect all unique XObject refs from all cached pages
        let mut xobj_refs: Vec<ObjectRef> = Vec::new();
        let page_dicts: Vec<Object> = self
            .page_cache
            .lock_or_recover()
            .values()
            .cloned()
            .collect();

        for page_obj in &page_dicts {
            let page_dict = match page_obj.as_dict() {
                Some(d) => d,
                None => continue,
            };
            let resources = match page_dict.get("Resources") {
                Some(r) => {
                    if let Some(ref_obj) = r.as_reference() {
                        match self.load_object(ref_obj) {
                            Ok(obj) => obj,
                            Err(_) => continue,
                        }
                    } else {
                        r.clone()
                    }
                }
                None => continue,
            };
            let res_dict = match resources.as_dict() {
                Some(d) => d,
                None => continue,
            };
            let xobj_obj = match res_dict.get("XObject") {
                Some(x) => {
                    if let Some(ref_obj) = x.as_reference() {
                        match self.load_object(ref_obj) {
                            Ok(obj) => obj,
                            Err(_) => continue,
                        }
                    } else {
                        x.clone()
                    }
                }
                None => continue,
            };
            if let Some(xobj_dict) = xobj_obj.as_dict() {
                for val in xobj_dict.values() {
                    if let Some(obj_ref) = val.as_reference() {
                        if !self
                            .image_xobject_cache
                            .lock_or_recover()
                            .contains(&obj_ref)
                        {
                            xobj_refs.push(obj_ref);
                        }
                    }
                }
            }
        }

        // Deduplicate
        xobj_refs.sort_unstable_by_key(|r| (r.id, r.gen));
        xobj_refs.dedup();

        // Sort by xref offset for sequential I/O
        xobj_refs.sort_by_key(|r| self.xref.get(r.id).map(|e| e.offset).unwrap_or(u64::MAX));

        log::debug!(
            "Prefetching XObject subtypes for {} unique refs",
            xobj_refs.len()
        );

        // Peek each ref — populates image_xobject_cache as a side effect
        for obj_ref in xobj_refs {
            self.is_form_xobject(obj_ref);
        }
    }

    /// Recursively walk the page tree and collect all pages into page_cache.
    fn collect_all_pages(
        &self,
        node_ref: ObjectRef,
        page_index: &mut usize,
        inherited: &mut HashMap<String, Object>,
        visited: &mut HashSet<ObjectRef>,
    ) -> Result<()> {
        if !visited.insert(node_ref) {
            return Err(Error::CircularReference(node_ref));
        }

        let node = self.load_object(node_ref)?;
        let node_dict = match node.as_dict() {
            Some(d) => d,
            None => return Ok(()), // Skip non-dict nodes gracefully
        };

        let node_type = node_dict
            .get("Type")
            .and_then(|obj| obj.as_name())
            .unwrap_or("");

        match node_type {
            "Page" => {
                // Apply inherited attributes
                let mut page_dict = node_dict.clone();
                for attr_name in &["Resources", "MediaBox", "CropBox", "Rotate"] {
                    if !page_dict.contains_key(*attr_name) {
                        if let Some(inherited_value) = inherited.get(*attr_name) {
                            log::debug!(
                                "Page {} inheriting {}: {:?}",
                                *page_index,
                                attr_name,
                                inherited_value
                            );
                            page_dict.insert(attr_name.to_string(), inherited_value.clone());
                        }
                    }
                }
                log::debug!(
                    "Collected page {}, keys: {:?}",
                    *page_index,
                    page_dict.keys()
                );
                if let Some(contents) = page_dict.get("Contents") {
                    log::debug!("  -> /Contents: {:?}", contents);
                }
                if let Some(rotate) = page_dict.get("Rotate") {
                    log::debug!("  -> /Rotate: {:?}", rotate);
                }
                self.page_cache
                    .lock_or_recover()
                    .insert(*page_index, Object::Dictionary(page_dict));
                *page_index += 1;
            }
            "Pages" => {
                // Save inherited state so siblings don't see each other's overrides
                let saved = inherited.clone();

                // Nearest ancestor's attributes override more distant ones (PDF spec §7.7.3.4).
                // insert() is correct here because we snapshot/restore `inherited` around
                // the recursion, so this node's values apply only to its subtree.
                for attr_name in &["Resources", "MediaBox", "CropBox", "Rotate"] {
                    if let Some(attr_value) = node_dict.get(*attr_name) {
                        log::debug!(
                            "Pages node at {:?} providing inheritable {}: {:?}",
                            node_ref,
                            attr_name,
                            attr_value
                        );
                        inherited.insert(attr_name.to_string(), attr_value.clone());
                    }
                }

                if let Some(kids) = node_dict.get("Kids").and_then(|obj| obj.as_array()) {
                    for kid in kids {
                        if let Some(kid_ref) = kid.as_reference() {
                            if let Err(e) =
                                self.collect_all_pages(kid_ref, page_index, inherited, visited)
                            {
                                log::warn!(
                                    "Error collecting page from tree: {}, skipping branch",
                                    e
                                );
                            }
                        }
                    }
                }

                *inherited = saved;
            }
            _ => {} // Unknown node type, skip
        }

        Ok(())
    }

    /// Get a page by scanning all objects in the PDF (fallback for broken page trees)
    /// This method is used when the standard page tree traversal fails due to malformed structure.
    fn get_page_by_scanning(&self, target_index: usize) -> Result<Object> {
        let mut current_index = 0;

        // Prime the ObjStm recovery cache up front when the xref looks
        // unreliable. Without this, the first pass below iterates only
        // `xref.all_object_numbers()` — which misses compressed objects
        // whose xref slots have been mis-flagged free. The sweep is a
        // one-shot, guarded by `objstm_recovery_done`, so this is cheap
        // if recovery already happened.
        self.recover_from_object_streams();

        // Collect all object numbers first to avoid borrow checker issues.
        // Sort for deterministic iteration order (HashMap iteration is
        // non-deterministic). We union the xref-listed ids with the object
        // ids recovered from the ObjStm sweep so that pages compressed in
        // streams whose xref slots were mis-flagged free still get visited.
        let mut obj_nums: Vec<u32> = self.xref.all_object_numbers().collect();
        for r in self.object_cache.lock_or_recover().keys() {
            obj_nums.push(r.id);
        }
        obj_nums.sort_unstable();
        obj_nums.dedup();

        // First pass: look for objects with /Type /Page
        for &obj_num in &obj_nums {
            if let Ok(obj) = self.load_object(ObjectRef {
                id: obj_num,
                gen: 0,
            }) {
                if let Some(dict) = obj.as_dict() {
                    if let Some(type_obj) = dict.get("Type") {
                        if let Some(type_name) = type_obj.as_name() {
                            if type_name == "Page" {
                                if current_index == target_index {
                                    return Ok(obj);
                                }
                                current_index += 1;
                            }
                        }
                    }
                }
            }
        }

        // Second pass: heuristic detection for pages without /Type entry.
        // Runs as a complement to pass 1 — counts page-like dicts that lack
        // a /Type entry alongside the /Type /Page matches, so that PDFs
        // whose corruption stripped /Type from some page dicts still reach
        // the full page count. Previously this pass only ran when pass 1
        // found zero pages, which meant any partial pass-1 match (e.g. 200
        // of 253 pages) would silently short pass 2 and fail.
        let mut heuristic_index = current_index;
        for &obj_num in &obj_nums {
            if let Ok(obj) = self.load_object(ObjectRef {
                id: obj_num,
                gen: 0,
            }) {
                if let Some(dict) = obj.as_dict() {
                    let has_no_type = dict.get("Type").is_none();
                    // Also handle /Type that is an unresolvable reference (Null)
                    let type_is_null = dict.get("Type").is_some_and(|t| matches!(t, Object::Null));
                    if (has_no_type || type_is_null)
                        && (dict.contains_key("MediaBox")
                            || dict.contains_key("Contents")
                            || (dict.contains_key("Resources") && dict.contains_key("Parent")))
                    {
                        log::debug!(
                            "Heuristic page candidate: object {} (page-like keys without valid /Type)",
                            obj_num
                        );
                        if heuristic_index == target_index {
                            return Ok(obj);
                        }
                        heuristic_index += 1;
                    }
                }
            }
        }
        current_index = heuristic_index;

        // Third pass: try resolving /Kids from catalog's /Pages root directly
        if current_index == 0 {
            if let Ok(catalog) = self.catalog() {
                if let Some(catalog_dict) = catalog.as_dict() {
                    if let Some(pages_ref) =
                        catalog_dict.get("Pages").and_then(|p| p.as_reference())
                    {
                        if let Ok(pages_obj) = self.load_object(pages_ref) {
                            if let Some(pages_dict) = pages_obj.as_dict() {
                                if let Some(kids) =
                                    pages_dict.get("Kids").and_then(|k| k.as_array())
                                {
                                    let mut kids_index = 0;
                                    for kid in kids {
                                        if let Some(kid_ref) = kid.as_reference() {
                                            // Skip self-referencing kids (cycle detection)
                                            if kid_ref == pages_ref {
                                                continue;
                                            }
                                            if let Ok(kid_obj) = self.load_object(kid_ref) {
                                                if let Some(kid_dict) = kid_obj.as_dict() {
                                                    // Skip intermediate /Pages nodes
                                                    let is_pages_node = kid_dict
                                                        .get("Type")
                                                        .and_then(|t| t.as_name())
                                                        .is_some_and(|n| n == "Pages");
                                                    if is_pages_node {
                                                        continue;
                                                    }
                                                    if kids_index == target_index {
                                                        log::debug!("Found page {} via direct /Kids resolution of object {}", target_index, kid_ref.id);
                                                        return Ok(kid_obj);
                                                    }
                                                    kids_index += 1;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Err(Error::InvalidPdf(format!(
            "Page index {} not found by scanning",
            target_index
        )))
    }

    /// Recursively traverse page tree to find a specific page.
    ///
    /// PDF Spec: ISO 32000-1:2008, Section 7.7.3.3 - Page Objects
    /// Implements attribute inheritance for /Resources, /MediaBox, /CropBox, /Rotate.
    ///
    /// Inheritable attributes from parent Pages nodes are collected as we traverse down
    /// the tree. When a Page is found, inherited attributes are merged in (only if the
    /// Page doesn't already have them - child values override parent values).
    fn get_page_from_tree(
        &self,
        node_ref: ObjectRef,
        target_index: usize,
        current_index: &mut usize,
        inherited: &mut HashMap<String, Object>,
    ) -> Result<Object> {
        self.get_page_from_tree_inner(
            node_ref,
            target_index,
            current_index,
            inherited,
            &mut HashSet::new(),
        )
    }

    fn get_page_from_tree_inner(
        &self,
        node_ref: ObjectRef,
        target_index: usize,
        current_index: &mut usize,
        inherited: &mut HashMap<String, Object>,
        visited: &mut HashSet<ObjectRef>,
    ) -> Result<Object> {
        if !visited.insert(node_ref) {
            return Err(Error::CircularReference(node_ref));
        }
        let node = self.load_object(node_ref)?;
        let node_dict = match node.as_dict() {
            Some(d) => d,
            None => {
                // Null or non-dict node in page tree — skip it
                log::warn!(
                    "Page tree node {} is {} (expected Dictionary), skipping",
                    node_ref.id,
                    node.type_name()
                );
                return Err(Error::InvalidPdf(format!(
                    "Page tree node {} is not a dictionary",
                    node_ref.id
                )));
            }
        };

        // Check if this is a page or pages node
        let node_type = node_dict
            .get("Type")
            .and_then(|obj| obj.as_name())
            .ok_or_else(|| Error::InvalidPdf("Page tree node missing /Type".to_string()))?;

        match node_type {
            "Pages" if *current_index < target_index => {
                // Skip entire subtree if /Count shows target is past this node.
                if let Some(count) = node_dict
                    .get("Count")
                    .and_then(|c| c.as_integer())
                    .filter(|&c| c > 0)
                {
                    let count = count as usize;
                    if *current_index + count <= target_index {
                        *current_index += count;
                        return Err(Error::InvalidPdf(format!(
                            "Page index {} not found in tree",
                            target_index
                        )));
                    }
                }
            }
            _ => {}
        }

        match node_type {
            "Page" => {
                if *current_index == target_index {
                    // Apply inherited attributes to this page
                    // PDF Spec: "If not present in the page dictionary, the value is inherited
                    // from an ancestor node in the page tree"
                    let mut page_dict = node_dict.clone();

                    // Inheritable attributes per PDF Spec Table 30:
                    // - Resources (required, can be inherited)
                    // - MediaBox (required, can be inherited)
                    // - CropBox (optional, can be inherited)
                    // - Rotate (optional, can be inherited)
                    let inheritable_attrs = ["Resources", "MediaBox", "CropBox", "Rotate"];

                    for attr_name in &inheritable_attrs {
                        // Only inherit if page doesn't already have this attribute
                        if !page_dict.contains_key(*attr_name) {
                            if let Some(inherited_value) = inherited.get(*attr_name) {
                                log::debug!(
                                    "Page {} inheriting /{} from ancestor Pages node",
                                    target_index,
                                    attr_name
                                );
                                page_dict.insert(attr_name.to_string(), inherited_value.clone());
                            }
                        }
                    }

                    Ok(Object::Dictionary(page_dict))
                } else {
                    *current_index += 1;
                    Err(Error::InvalidPdf(format!(
                        "Page index {} not found in tree",
                        target_index
                    )))
                }
            }
            "Pages" => {
                // This is an intermediate Pages node with kids
                // Collect inheritable attributes from this node to pass to children
                let inheritable_attrs = ["Resources", "MediaBox", "CropBox", "Rotate"];

                for attr_name in &inheritable_attrs {
                    if let Some(attr_value) = node_dict.get(*attr_name) {
                        // Only add if not already in inherited map (child values override parent)
                        inherited
                            .entry(attr_name.to_string())
                            .or_insert_with(|| attr_value.clone());
                    }
                }

                // Try to get /Kids array; if missing, this is a malformed PDF
                let kids = match node_dict.get("Kids").and_then(|obj| obj.as_array()) {
                    Some(k) => k,
                    None => {
                        log::warn!("Malformed PDF: Pages node missing /Kids array");
                        // Malformed PDF: Pages node has no /Kids array
                        // Gracefully return without error to allow other recovery paths
                        // The scanning method will find pages eventually
                        return Err(Error::InvalidPdf(
                            "Pages node missing /Kids array - try fallback method".to_string(),
                        ));
                    }
                };

                for kid in kids {
                    let kid_ref = kid.as_reference().ok_or_else(|| {
                        Error::InvalidPdf("Kid in /Kids array is not a reference".to_string())
                    })?;

                    match self.get_page_from_tree_inner(
                        kid_ref,
                        target_index,
                        current_index,
                        inherited,
                        visited,
                    ) {
                        Ok(page) => return Ok(page),
                        Err(Error::CircularReference(obj_ref)) => {
                            log::warn!(
                                "Circular reference in page tree at object {}, skipping",
                                obj_ref
                            );
                            continue;
                        }
                        Err(Error::RecursionLimitExceeded(_)) => {
                            log::warn!("Recursion limit exceeded in page tree, skipping branch");
                            continue;
                        }
                        Err(_) => continue,
                    }
                }

                Err(Error::InvalidPdf(format!(
                    "Page index {} not found",
                    target_index
                )))
            }
            _ => Err(Error::InvalidPdf(format!(
                "Unknown page tree node type: {}",
                node_type
            ))),
        }
    }

    /// Get the object reference for a page by index.
    ///
    /// This is used by outline and annotations to find page references.
    pub(crate) fn get_page_ref(&self, page_index: usize) -> Result<ObjectRef> {
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;

        let pages_ref = catalog_dict
            .get("Pages")
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?
            .as_reference()
            .ok_or_else(|| Error::InvalidPdf("/Pages is not a reference".to_string()))?;

        self.get_page_ref_recursive(pages_ref, page_index, &mut 0, &mut HashSet::new())
    }

    /// Recursively find page reference in the page tree.
    pub(crate) fn get_page_ref_recursive(
        &self,
        node_ref: ObjectRef,
        target_index: usize,
        current_index: &mut usize,
        visited: &mut HashSet<ObjectRef>,
    ) -> Result<ObjectRef> {
        if !visited.insert(node_ref) {
            return Err(Error::CircularReference(node_ref));
        }
        let node = self.load_object(node_ref)?;
        let node_dict = match node.as_dict() {
            Some(d) => d,
            None => {
                log::warn!(
                    "Page tree node {} is {} (expected Dictionary), skipping",
                    node_ref.id,
                    node.type_name()
                );
                return Err(Error::InvalidPdf(format!(
                    "Page tree node {} is not a dictionary",
                    node_ref.id
                )));
            }
        };

        let node_type = node_dict
            .get("Type")
            .and_then(|t| t.as_name())
            .ok_or_else(|| Error::InvalidPdf("Node missing Type".to_string()))?;

        match node_type {
            "Page" => {
                if *current_index == target_index {
                    Ok(node_ref)
                } else {
                    *current_index += 1;
                    Err(Error::InvalidPdf(format!(
                        "Page {} not found",
                        target_index
                    )))
                }
            }
            "Pages" => {
                let kids = node_dict
                    .get("Kids")
                    .and_then(|k| k.as_array())
                    .ok_or_else(|| Error::InvalidPdf("Pages node missing Kids".to_string()))?;

                for kid_obj in kids {
                    if let Some(kid_ref) = kid_obj.as_reference() {
                        match self.get_page_ref_recursive(
                            kid_ref,
                            target_index,
                            current_index,
                            visited,
                        ) {
                            Ok(page_ref) => return Ok(page_ref),
                            Err(_) => continue,
                        }
                    }
                }

                Err(Error::InvalidPdf(format!(
                    "Page {} not found",
                    target_index
                )))
            }
            _ => Err(Error::InvalidPdf(format!(
                "Unknown node type: {}",
                node_type
            ))),
        }
    }

    /// Extract text from a page as a plain string.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Returns
    ///
    /// The extracted text as a string.
    ///
    /// # Errors
    ///
    /// Returns an error if the page cannot be accessed or text extraction fails.
    /// Decode PDF escape sequences in text (e.g., \274 -> §, \( -> (, etc.)
    #[allow(dead_code)]
    fn decode_pdf_escapes(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '\\' {
                // Check what follows the backslash
                match chars.peek() {
                    Some(&'(') => {
                        result.push('(');
                        chars.next();
                    }
                    Some(&')') => {
                        result.push(')');
                        chars.next();
                    }
                    Some(&'\\') => {
                        result.push('\\');
                        chars.next();
                    }
                    Some(&'n') => {
                        result.push('\n');
                        chars.next();
                    }
                    Some(&'r') => {
                        result.push('\r');
                        chars.next();
                    }
                    Some(&'t') => {
                        result.push('\t');
                        chars.next();
                    }
                    Some(&'?') => {
                        // \? is a soft hyphen (optional line break point)
                        // Just skip it
                        chars.next();
                    }
                    Some(d) if d.is_ascii_digit() => {
                        // Octal escape sequence: \ddd
                        let mut octal = String::new();
                        for _ in 0..3 {
                            if let Some(&digit) = chars.peek() {
                                if digit.is_ascii_digit() && digit < '8' {
                                    octal.push(digit);
                                    chars.next();
                                } else {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }

                        if !octal.is_empty() {
                            if let Ok(code) = u8::from_str_radix(&octal, 8) {
                                // PDFDocEncoding: ISO 32000-1:2008, Annex D
                                let decoded_char = Self::pdfdoc_decode(code);
                                result.push(decoded_char);
                            } else {
                                // Failed to parse, keep the backslash and octal
                                result.push('\\');
                                result.push_str(&octal);
                            }
                        } else {
                            // No valid octal digits, keep the backslash
                            result.push('\\');
                        }
                    }
                    _ => {
                        // Unknown escape, keep the backslash
                        result.push('\\');
                    }
                }
            } else {
                result.push(ch);
            }
        }

        result
    }

    /// Decode a byte using PDFDocEncoding (ISO 32000-1:2008, Annex D).
    ///
    /// PDFDocEncoding is the default encoding for text strings in PDF:
    /// - Codes 0-127: ASCII
    /// - Codes 128-159: Special Unicode characters
    /// - Codes 160-255: Latin-1 (ISO 8859-1)
    #[allow(dead_code)]
    fn pdfdoc_decode(code: u8) -> char {
        match code {
            // 0-127: Standard ASCII
            0..=127 => code as char,

            // 128-159: PDFDocEncoding special mappings
            128 => '\u{2022}', // BULLET
            129 => '\u{2020}', // DAGGER
            130 => '\u{2021}', // DOUBLE DAGGER
            131 => '\u{2026}', // HORIZONTAL ELLIPSIS
            132 => '\u{2014}', // EM DASH
            133 => '\u{2013}', // EN DASH
            134 => '\u{0192}', // LATIN SMALL LETTER F WITH HOOK
            135 => '\u{2044}', // FRACTION SLASH
            136 => '\u{2039}', // SINGLE LEFT-POINTING ANGLE QUOTATION MARK
            137 => '\u{203A}', // SINGLE RIGHT-POINTING ANGLE QUOTATION MARK
            138 => '\u{2212}', // MINUS SIGN
            139 => '\u{2030}', // PER MILLE SIGN
            140 => '\u{201E}', // DOUBLE LOW-9 QUOTATION MARK
            141 => '\u{201C}', // LEFT DOUBLE QUOTATION MARK
            142 => '\u{201D}', // RIGHT DOUBLE QUOTATION MARK
            143 => '\u{2018}', // LEFT SINGLE QUOTATION MARK
            144 => '\u{2019}', // RIGHT SINGLE QUOTATION MARK
            145 => '\u{201A}', // SINGLE LOW-9 QUOTATION MARK
            146 => '\u{2122}', // TRADE MARK SIGN
            147 => '\u{FB01}', // LATIN SMALL LIGATURE FI
            148 => '\u{FB02}', // LATIN SMALL LIGATURE FL
            149 => '\u{0141}', // LATIN CAPITAL LETTER L WITH STROKE
            150 => '\u{0152}', // LATIN CAPITAL LIGATURE OE
            151 => '\u{0160}', // LATIN CAPITAL LETTER S WITH CARON
            152 => '\u{0178}', // LATIN CAPITAL LETTER Y WITH DIAERESIS
            153 => '\u{017D}', // LATIN CAPITAL LETTER Z WITH CARON
            154 => '\u{0131}', // LATIN SMALL LETTER DOTLESS I
            155 => '\u{0142}', // LATIN SMALL LETTER L WITH STROKE
            156 => '\u{0153}', // LATIN SMALL LIGATURE OE
            157 => '\u{0161}', // LATIN SMALL LETTER S WITH CARON
            158 => '\u{017E}', // LATIN SMALL LETTER Z WITH CARON
            159 => '\u{FFFD}', // REPLACEMENT CHARACTER (undefined in PDFDocEncoding)

            // 160-255: Latin-1 (ISO 8859-1)
            160..=255 => code as char,
        }
    }

    /// Circular references and recursion limit errors are handled gracefully
    /// with warning messages in the output.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let text = doc.extract_text(0)?;
    /// println!("Page 1 text: {}", text);
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    ///
    /// # Extract text from a page
    ///
    /// pdf_oxide exposes three plain-text surfaces with different strengths
    /// (#554). Pick by document shape:
    ///
    /// - `extract_text(page)` (this method) — glyph-walk assembly with
    ///   row-aware ordering, inline table rendering, and artifact filtering.
    ///   The most discoverable default; strongest on single-column prose.
    /// - `to_plain_text(page, opts)` / `to_plain_text_all(opts)` — runs the
    ///   full pipeline (reading-order strategy incl. XY-cut). Best on
    ///   multi-column / complex layouts where reading order matters.
    /// - `to_markdown_all(opts)` then strip markup — preserves structure
    ///   (headings, lists, tables) and often scores highest on heavily
    ///   structured documents; lossiest for pure prose.
    ///
    /// No single mode wins on every PDF; when extraction quality is critical
    /// and the layout is unknown, compare `to_plain_text_all` and
    /// markdown-stripped output and keep whichever is better for your corpus.
    pub fn extract_text(&self, page_index: usize) -> Result<String> {
        // Enable table extraction so that tabular content is preserved as
        // space-padded, column-aligned rows (see Table::render_text).
        let options = crate::converters::ConversionOptions {
            extract_tables: true,
            ..Default::default()
        };
        self.extract_text_with_options(page_index, &options)
    }

    /// Extract text from a page with specific options (v0.3.16).
    pub fn extract_text_with_options(
        &self,
        page_index: usize,
        options: &crate::converters::ConversionOptions,
    ) -> Result<String> {
        let base_spans = self.extract_spans(page_index)?;
        // Vertical CJK (tategaki, ISO 32000-1 §9.7.4.3 vertical writing mode):
        // glyphs run top-to-bottom in columns that progress right-to-left, so
        // the horizontal row-major assembler shreds the reading order. When the
        // page is geometrically vertical, read it column-major instead.
        if let Some(vertical) = Self::try_assemble_vertical_cjk(&base_spans) {
            return Ok(vertical);
        }
        // Dominant text-matrix rotation (a landscape table typeset on a
        // portrait page): the row-major assembler groups lines
        // in the portrait frame and interleaves every rotated row. Assemble
        // such pages in their rotated reading frame instead.
        let base_spans = match self.map_dominant_rotation_into_reading_frame(page_index, base_spans)
        {
            Ok(mapped) => mapped,
            Err(original) => original,
        };
        let text = self.assemble_text_from_spans(page_index, base_spans, options)?;
        Ok(Self::apply_mixed_rtl_line_pass(text))
    }

    /// Map a dominant-rotation page's spans into their rotated reading
    /// frame so the standard horizontal assembler applies.
    ///
    /// Returns `Ok(mapped)` when the page is unrotated (`/Rotate 0` — on
    /// rotated pages `postprocess_spans` already handles content rotation,
    ///) and at least half its non-whitespace spans share one
    /// quadrant text rotation; the mapped spans are horizontal in the
    /// frame a reader turns the page into, with `rotation_degrees` cleared
    /// so downstream passes treat them as the upright text they now are.
    /// Returns `Err(spans)` — the input unchanged — on every other page,
    /// keeping output byte-identical there.
    ///
    /// Only used for plain-text assembly, where no coordinates leak to the
    /// caller; coordinate-bearing APIs (`extract_words`) reorder in the
    /// rotated frame but report true page-space bboxes instead (see
    /// `crate::pipeline::page_reading_order`).
    fn map_dominant_rotation_into_reading_frame(
        &self,
        page_index: usize,
        spans: Vec<crate::layout::TextSpan>,
    ) -> std::result::Result<Vec<crate::layout::TextSpan>, Vec<crate::layout::TextSpan>> {
        if self.get_page_rotation(page_index).unwrap_or(0) != 0 {
            return Err(spans);
        }
        let Some(deg) = crate::utils::dominant_rotation(&spans) else {
            return Err(spans);
        };
        // Same quadrant mapping as the word path: 90° text reads upright
        // under a /Rotate-90-style display transform, -90° under 270,
        // 180° under 180. Mirrored / free-angle runs have no frame.
        let rot = if (deg - 90.0).abs() < 0.5 {
            90
        } else if (deg - 180.0).abs() < 0.5 {
            180
        } else if (deg + 90.0).abs() < 0.5 {
            270
        } else {
            return Err(spans);
        };
        log::debug!(
            "page {page_index}: dominant text rotation {deg}° — assembling text in rotated frame"
        );
        let (llx, lly, urx, ury) = self
            .get_page_media_box(page_index)
            .unwrap_or((0.0, 0.0, 612.0, 792.0));
        let (w, h) = (urx - llx, ury - lly);
        let mut spans = spans;
        // Rotated spans store TEXT-LOCAL extents (origin + advance-along-
        // the-run as `width` + font size as `height`): rotate the ORIGIN
        // as a point and keep the extents, which already describe the run
        // in its own upright frame (same convention as
        // `order_rotated_blocks`).
        for s in &mut spans {
            let (rx, ry) = (s.bbox.x - llx, s.bbox.y - lly);
            let (mx, my) = match rot {
                90 => (ry, w - rx),
                180 => (w - rx, h - ry),
                270 => (h - ry, rx),
                _ => (rx, ry),
            };
            s.bbox.x = llx + mx;
            s.bbox.y = lly + my;
            s.rotation_degrees = 0.0;
        }
        Ok(spans)
    }

    /// Returns column-major text when the page is a vertical-CJK (tategaki)
    /// layout, or `None` for every other page (so horizontal documents are
    /// byte-for-byte unchanged).
    ///
    /// Detection is purely geometric: among CJK glyph spans, count how many
    /// neighbour pairs are stacked *vertically* (same column, one glyph-height
    /// apart) versus *horizontally* (same row, one glyph-width apart). Vertical
    /// writing is declared only when CJK is the clear majority of the page and
    /// vertical adjacencies dominate horizontal ones — so horizontal CJK
    /// (Chinese/Japanese prose set left-to-right) never triggers it. Assembly
    /// then orders spans by column right-to-left (X descending, banded to the
    /// glyph width) and top-to-bottom within a column (Y descending), matching
    /// how the script is read.
    fn try_assemble_vertical_cjk(spans: &[TextSpan]) -> Option<String> {
        fn is_cjk(c: char) -> bool {
            matches!(
                c as u32,
                0x3040..=0x30FF      // Hiragana + Katakana
                | 0x3400..=0x4DBF    // CJK Ext A
                | 0x4E00..=0x9FFF    // CJK Unified
                | 0xF900..=0xFAFF    // CJK Compatibility
                | 0xFF66..=0xFF9F    // Halfwidth Katakana
            )
        }
        let cjk: Vec<&TextSpan> = spans
            .iter()
            .filter(|s| s.text.chars().any(is_cjk))
            .collect();
        if cjk.len() < 8 {
            return None;
        }
        // Tategaki signature: vertical writing positions each glyph on its own
        // origin, so a genuine vertical-CJK page is composed of SINGLE-glyph
        // spans. Horizontal CJK is emitted as multi-character runs (a whole
        // word/line per show op). The column-major geometry below assumes glyph
        // cells; on run-level spans the nearest neighbour of a run is the run on
        // the line above/below (vertical), so a horizontal page is mis-detected
        // as vertical and its reading order is shredded. Require single-glyph
        // CJK spans to be the majority before treating the page as vertical;
        // otherwise fall back to the horizontal assembler (the pre-vertical-CJK
        // behaviour, so a missed detection never regresses against it).
        let single_glyph_cjk = cjk
            .iter()
            .filter(|s| {
                let t = s.text.trim();
                t.chars().count() == 1 && t.chars().all(is_cjk)
            })
            .count();
        if single_glyph_cjk * 2 < cjk.len() {
            return None;
        }
        // CJK must be the clear majority of the page's non-space glyphs.
        let total_chars: usize = spans
            .iter()
            .map(|s| s.text.chars().filter(|c| !c.is_whitespace()).count())
            .sum();
        let cjk_chars: usize = cjk
            .iter()
            .map(|s| s.text.chars().filter(|c| is_cjk(*c)).count())
            .sum();
        if total_chars == 0 || cjk_chars * 2 < total_chars {
            return None;
        }

        // Glyph cell width from the median-ish span box (CJK glyphs are square);
        // used to band columns when sorting column-major below.
        let mut widths: Vec<f32> = cjk
            .iter()
            .map(|s| s.bbox.width)
            .filter(|w| *w > 0.0)
            .collect();
        if widths.is_empty() {
            return None;
        }
        widths.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        let gw = widths[widths.len() / 2];

        // Discriminate vertical (tategaki) from horizontal CJK by each glyph's
        // NEAREST-neighbour direction (capped for O(n²) cost). The earlier
        // all-pairs `vert > horiz` count mis-classified grid-aligned HORIZONTAL
        // CJK — genkō-yōshi regulatory/academic text aligns glyphs in both rows
        // and columns, so vertical and horizontal adjacencies are about equal
        // and noise tipped the page to "vertical", scrambling its reading order
        // and collapsing line breaks. A glyph's single closest neighbour is the
        // next glyph in its own line: horizontally adjacent for horizontal text
        // (intra-row pitch < inter-row leading), vertically adjacent for
        // tategaki (intra-column pitch < inter-column gutter). Only assemble
        // column-major when vertical nearest-neighbours clearly dominate
        // (> 2×). Otherwise fall back to the normal horizontal path — which is
        // also the pre-vertical-CJK (≤ 0.3.61) behaviour, so a missed detection
        // never regresses against that baseline.
        let sample = &cjk[..cjk.len().min(250)];
        let (mut vert, mut horiz) = (0usize, 0usize);
        for (i, a) in sample.iter().enumerate() {
            let (mut best, mut bdx, mut bdy) = (f32::MAX, 0.0f32, 0.0f32);
            for (j, b) in sample.iter().enumerate() {
                if i == j {
                    continue;
                }
                let dx = a.bbox.x - b.bbox.x;
                let dy = a.bbox.y - b.bbox.y;
                let d2 = dx * dx + dy * dy;
                if d2 < best {
                    best = d2;
                    bdx = dx.abs();
                    bdy = dy.abs();
                }
            }
            // Classify the nearest neighbour's dominant axis (ties → neither).
            if bdy > bdx {
                vert += 1;
            } else if bdx > bdy {
                horiz += 1;
            }
        }
        // Require a clear vertical majority; ambiguous or horizontal → None.
        if vert == 0 || vert <= horiz * 2 {
            return None;
        }

        // Column-major order: X descending (right-to-left), banded to the glyph
        // width so a column's sub-pixel X jitter does not split it, then Y
        // descending (top-to-bottom) within the column.
        let band = (gw * 0.5).max(1.0);
        let mut ordered: Vec<&TextSpan> = spans.iter().collect();
        ordered.sort_by(|a, b| {
            let ca = (a.bbox.x / band).round() as i32;
            let cb = (b.bbox.x / band).round() as i32;
            cb.cmp(&ca)
                .then(crate::utils::safe_float_cmp(b.bbox.y, a.bbox.y))
        });
        Some(ordered.iter().map(|s| s.text.as_str()).collect())
    }

    /// Per-line UAX #9 pass for mixed-direction lines (bidi item 4): for each
    /// output line that is confidently RTL and mixes Arabic/Hebrew with
    /// European/Arabic-Indic numerals or Latin words (e.g. a date
    /// `14 april 1434 ٤٣٤١`), give the embedded LTR sub-runs their left-to-right
    /// sublevel (UAX #9 §3.3.4) while leaving the already-logical RTL runs fixed.
    /// Gated inside `reorder_mixed_rtl_line`, so pure-RTL, pure-LTR, and
    /// non-RTL lines are returned byte-for-byte unchanged; the ASCII fast path
    /// keeps all Latin-only extraction identical.
    fn apply_mixed_rtl_line_pass(text: String) -> String {
        if text.is_ascii() {
            return text;
        }
        text.split('\n')
            .map(crate::text::bidi::reorder_mixed_rtl_line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Apply caller-specified region filters to a span set: drop spans matching
    /// any `exclude_regions` (under `exclude_regions_mode`), then keep only spans
    /// inside `include_region` if one is set. Exclusion runs first so it takes
    /// precedence. Shared by the plain-text, markdown, and HTML conversion paths
    /// so `ConversionOptions` region filtering behaves identically across every
    /// text surface. A no-op when neither field is set.
    fn apply_region_filters(
        base_spans: Vec<crate::layout::TextSpan>,
        options: &crate::converters::ConversionOptions,
    ) -> Vec<crate::layout::TextSpan> {
        use crate::layout::SpatialCollectionFiltering;
        let mut spans = base_spans;
        if !options.exclude_regions.is_empty() {
            spans = spans.exclude_rects(&options.exclude_regions, options.exclude_regions_mode);
        }
        if let Some((ref region, mode)) = options.include_region {
            spans = spans.filter_by_rect(region, mode);
        }
        spans
    }

    fn assemble_text_from_spans(
        &self,
        page_index: usize,
        base_spans: Vec<crate::layout::TextSpan>,
        options: &crate::converters::ConversionOptions,
    ) -> Result<String> {
        if self.is_encrypted_unreadable() {
            log::warn!("PDF is encrypted and could not be decrypted; returning empty text");
            return Ok(String::new());
        }

        let base_spans = Self::apply_region_filters(base_spans, options);
        // Struct-tree-scope `/ActualText` is applied per branch below
        // — the structure-order assembler handles it natively via the
        // per-page action map, and the geometric branch applies the
        // raw-span applier on its own input. Pre-applying here would
        // double-process: the structure-order path would see already-
        // mutated spans and lose run-position information, dropping
        // sibling MCIDs of a nested scope (CRITICAL-1 shape).

        // Structure tree: use it for reading order only when it is trustworthy
        // per the shared predicate (§14.8.2.3.1) — the document is /Marked or
        // the catalog references a /StructTreeRoot (PDF 1.4 documents such as
        // hello_structure.pdf predate /MarkInfo but are still tagged, §14.7.1),
        // AND /MarkInfo /Suspects is not true. Suspect documents fall through to
        // the geometric `else` arm below, the spec-correct behaviour.
        let cached_tree = self.struct_tree_trustworthy();
        let widget_spans = self.extract_widget_spans(page_index);

        // Table detection uses base spans only (no widget spans).
        let tables = if options.extract_tables {
            // text_fallback=false: extract_text preserves the pre-v0.3.47 behaviour
            // where line-less pages return no tables. Only the structured-output
            // converters (to_markdown, to_html) opt in to text-only spatial fallback.
            self.extract_page_tables(page_index, &base_spans, options, false)
        } else {
            Vec::new()
        };

        let mut all_spans = base_spans;
        all_spans.extend(widget_spans);

        if all_spans.is_empty() {
            let page = self.get_page(page_index)?;
            let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
                offset: 0,
                reason: "Page is not a dictionary".to_string(),
            })?;
            let no_content_text = if self.page_cannot_have_text(page_dict) {
                true
            } else {
                // Also check content stream for BT/Do operators (SIMD-fast scan).
                match self.get_page_content_data(page_index) {
                    Ok(ref content_data) => !Self::may_contain_text(content_data),
                    Err(_) => false, // Can't read content stream — be conservative
                }
            };
            if no_content_text {
                let mut text = String::new();
                self.append_non_widget_annotation_text(page_index, &mut text);
                return Ok(text);
            }
        }

        let text = if let Some(ref struct_tree) = cached_tree {
            // Build per-page traversal cache once, then O(1) lookup per page.
            if self.structure_content_cache.lock_or_recover().is_none() {
                let all_content = crate::structure::traverse_structure_tree_all_pages(struct_tree);
                *self.structure_content_cache.lock_or_recover() = Some(all_content);
            }
            self.extract_text_structure_order_cached_with_spans(
                page_index,
                all_spans,
                options.include_artifacts,
            )?
        } else {
            // Untagged or Suspects=true PDF: use page content
            // (geometric) order. Apply struct-tree-scope `/ActualText`
            // here — the structure-order assembler above handles it
            // natively for the trustworthy branch. Suspects=true
            // documents still get their producer-supplied replacement
            // because `actualtext_index()` is decoupled from
            // `struct_tree_marked` (§14.9.4 is content replacement,
            // not a reading-order signal).
            let mut spans = all_spans;
            self.apply_actualtext_to_spans(page_index, &mut spans);

            // Exclude spans that are inside detected tables, BUT
            // preserve multi-row-spanning label columns.
            // The spatial table extractor clusters data cells into
            // table cells but does NOT emit the sparse label column
            // that sits vertically centred within each multi-row data
            // block (common on CJK lab-report reference tables like
            // WS/T 779). Those labels would otherwise be dropped
            // entirely from the output: the retain below would remove
            // them because their bbox is inside the table,
            // `table.render_text()` would not re-emit them because the
            // extractor never captured them as cells. Before running
            // the retain filter we identify these rowspan labels (same
            // heuristic `reorder_rowspan_labels` uses) and keep them in
            // the span list so `reorder_rowspan_labels` below can
            // promote them to the top of their row block.
            if !tables.is_empty() {
                // Absorb floating-point accumulation error in the
                // difference between a span's directly-computed
                // bbox.right (origin + width, small accumulation)
                // and a table bbox.right (min/max reduction across
                // many cell edges, larger accumulation). Without
                // this slack, a span whose real geometry is inside
                // the table by construction but whose f32 right-edge
                // exceeds the table's f32 right-edge by ~0.01–0.05pt
                // gets wrongly kept in the flow stream, producing
                // duplicated output. 0.1pt is well below any
                // visually meaningful PDF layout distance.
                const RETAIN_TOLERANCE: f32 = 0.1;

                // Build the set of cell text strings that every detected
                // table will render via `table.render_text()`. Labels
                // whose exact text already appears as a cell in some
                // table are already covered by the inline-table flush
                // below, so we must NOT also preserve them in the flow
                // span list (it would produce duplicate output).
                let mut table_cell_texts: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for t in &tables {
                    for row in &t.rows {
                        for cell in &row.cells {
                            let trimmed = cell.text.trim();
                            if !trimmed.is_empty() {
                                table_cell_texts.insert(trimmed.to_string());
                            }
                        }
                    }
                }

                // For tagged PDFs, collect the MCIDs that are actually owned by
                // table cells. When a span's MCID is NOT in this set, the span is
                // NOT part of the table even if it lies inside the table's bbox
                // (e.g. a paragraph physically adjacent to a table that was tagged
                // as a sibling <P> element, not as a <TD>). Filtering such spans
                // by bbox alone would silently drop real content.
                // Falls back to bbox-only filtering when no MCIDs are present
                // (untagged PDFs or spatial-detection tables).
                let table_cell_mcids: HashSet<u32> = tables
                    .iter()
                    .flat_map(|t| {
                        t.rows
                            .iter()
                            .flat_map(|r| r.cells.iter().flat_map(|c| c.mcids.iter().copied()))
                    })
                    .collect();
                // Flatten every cell bbox once and index it into coarse y-bands,
                // so the per-span containment test below scans only the cells in
                // the span's y-band instead of every cell on the page (was
                // O(spans x cells) on untagged table pages). A cell that contains
                // a span necessarily shares the span's y-band, so this is
                // byte-identical to the full scan.
                let cell_bboxes: Vec<crate::geometry::Rect> = tables
                    .iter()
                    .flat_map(|t| {
                        t.rows
                            .iter()
                            .flat_map(|r| r.cells.iter().filter_map(|c| c.bbox))
                    })
                    .collect();
                const CELL_Y_BIN: f32 = 18.0;
                let cell_bin = |y: f32| (y / CELL_Y_BIN).floor() as i32;
                let mut cell_y_index: std::collections::HashMap<i32, Vec<usize>> =
                    std::collections::HashMap::new();
                for (ci, b) in cell_bboxes.iter().enumerate() {
                    for bin in cell_bin(b.y)..=cell_bin(b.y + b.height) {
                        cell_y_index.entry(bin).or_default().push(ci);
                    }
                }
                // Returns true when span should be removed from the flow because
                // it is owned by a table cell (will be re-emitted by render_text).
                let span_in_table = |s: &crate::layout::TextSpan| -> bool {
                    if !table_cell_mcids.is_empty() {
                        if let Some(mcid) = s.mcid {
                            // Tagged PDF: MCID decides ownership precisely.
                            return table_cell_mcids.contains(&mcid);
                        }
                        // Tagged PDF but span has no MCID (widget/annotation):
                        // keep in flow — better to duplicate than to silently drop.
                        return false;
                    }
                    // Untagged PDF or no MCIDs in any cell: cell-bbox-based filter.
                    // Using per-cell bboxes (rather than the coarser table bbox) prevents
                    // dropping paragraph spans that lie inside the table's outer bounding
                    // box but were not captured as table cells by the spatial detector.
                    // Probe only the cells in the span's y-band (±1 bin guards the
                    // containment tolerance). Equivalent to scanning every cell.
                    let slo = cell_bin(s.bbox.y) - 1;
                    let shi = cell_bin(s.bbox.y + s.bbox.height) + 1;
                    let in_cell = (slo..=shi).any(|bin| {
                        cell_y_index.get(&bin).is_some_and(|cands| {
                            cands.iter().any(|&ci| {
                                Self::contains_rect_with_tolerance(
                                    &cell_bboxes[ci],
                                    &s.bbox,
                                    RETAIN_TOLERANCE,
                                )
                            })
                        })
                    });
                    if in_cell {
                        return true;
                    }
                    // Fallback: text-based match. The bbox check above uses
                    // a tight 0.1pt tolerance and rejects spans whose font
                    // ascent extends slightly above the cell's ink box (issue
                    // 484: "FY 15 1st Q TTL" labels in JAL traffic table —
                    // span height = font_size = 10.7pt, but cell bbox height
                    // = 15.96pt covers two ink rows so the label glyphs reach
                    // ~0.4pt above the cell's top edge). When a span's
                    // trimmed text exactly matches some cell's text, the cell
                    // already owns it — keeping it in flow would duplicate.
                    let trimmed = s.text.trim();
                    if trimmed.is_empty() {
                        return false;
                    }
                    if !table_cell_texts.contains(trimmed) {
                        return false;
                    }
                    // Require spatial proximity: the span must lie inside
                    // some table's outer bbox so we don't drop body text that
                    // coincidentally matches a cell's text elsewhere on the page.
                    tables.iter().any(|t| {
                        t.bbox.is_some_and(|tb| {
                            let cx = s.bbox.x + s.bbox.width / 2.0;
                            let cy = s.bbox.y + s.bbox.height / 2.0;
                            cx >= tb.x - RETAIN_TOLERANCE
                                && cx <= tb.x + tb.width + RETAIN_TOLERANCE
                                && cy >= tb.y - RETAIN_TOLERANCE
                                && cy <= tb.y + tb.height + RETAIN_TOLERANCE
                        })
                    })
                };

                let preserved_label_indices: std::collections::HashSet<usize> =
                    Self::identify_multi_row_labels(&spans)
                        .into_iter()
                        .filter(|&idx| {
                            // Only preserve labels whose text is NOT
                            // already emitted by any table's
                            // `render_text()`. This is what makes the
                            // #329 fix safe on pages where the spatial
                            // extractor captured the sparse label
                            // column as cells — we let the table
                            // render them and drop them from flow.
                            // On pages like WS/T 779 where the label
                            // column is a genuine multi-row-spanning
                            // column that the extractor did NOT
                            // capture, the set is empty and every
                            // identified label stays in flow where
                            // `reorder_rowspan_labels` below can
                            // promote it.
                            let t = spans[idx].text.trim();
                            !t.is_empty() && !table_cell_texts.contains(t)
                        })
                        .collect();

                if preserved_label_indices.is_empty() {
                    spans.retain(|s| !span_in_table(s));
                } else {
                    let kept: Vec<crate::layout::TextSpan> = spans
                        .drain(..)
                        .enumerate()
                        .filter_map(|(i, s)| {
                            if !span_in_table(&s) || preserved_label_indices.contains(&i) {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .collect();
                    spans = kept;
                }
            }

            // Row-aware ordering: quantize Y into bands and sort band-
            // descending, then X ascending within a band. Strict Y sorting
            // would interleave cells from the same tabular row whose Y
            // values differ by typographic jitter (common in CJK layouts,
            // superscripts, and centered multi-line labels).
            //
            // Skip for multi-column pages: extract_spans() already applied
            // XY-cut column ordering. Re-sorting with row-aware would interleave
            // left/right columns line-by-line, splicing words from adjacent
            // columns into each other. A topological block order (a precede
            // relation over text blocks) handles genuine multi-region pages (a
            // two-column body/footer, a sidebar beside the body) that a flat
            // row-aware (y,x) sort interleaves. The gate (substantial, text-dense,
            // dominant side-by-side blocks) rejects single-column pages, tables,
            // TOCs and forms; it de-interleaves real two-column bodies and
            // sidebar+body layouts. topological_block_order runs unconditionally;
            // it self-gates to None unless the page has dominant side-by-side
            // text-dense blocks (see its side_by_side gate), so single-column,
            // table, and TOC pages are byte-identical.
            //
            // Item 2 (M2): first lift a narrow, sparse, body-aligned marginalia
            // rail (manuscript line numbers / a folio rail) OUT of the body, so
            // the column dispatch below sees a clean body. A rail otherwise
            // injects a spurious second corridor (disqualifying prose detection)
            // and a sparse block (defeating the topological side-by-side gate),
            // and a flat (y,x) sort then weaves its numerals into the prose. The
            // rail is re-appended at the end of the reading order after the
            // ladder, before the artifact retain. No-op (None) on ordinary pages
            // → byte-identical.
            let marginalia_trailing: Vec<crate::layout::TextSpan> =
                if let Some(idx) = Self::lift_marginalia_column(&spans) {
                    let idxset: std::collections::HashSet<usize> = idx.into_iter().collect();
                    let mut keep = Vec::with_capacity(spans.len());
                    let mut marg = Vec::new();
                    for (i, s) in std::mem::take(&mut spans).into_iter().enumerate() {
                        if idxset.contains(&i) {
                            marg.push(s);
                        } else {
                            keep.push(s);
                        }
                    }
                    marg.sort_by(|a, b| {
                        crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
                    });
                    spans = keep;
                    marg
                } else {
                    Vec::new()
                };

            let mut topo_applied = false;
            if let Some(reordered) = Self::topological_block_order(&spans) {
                spans = reordered;
                topo_applied = true;
            }
            if topo_applied {
                // Topological block order replaced the row-aware sort.
            } else if let Some(ordered) = Self::sidebar_body_reading_order(&spans) {
                // Narrow metadata SIDEBAR + wide body (e.g. an MDPI first page
                // whose left rail carries Citation:/Received:/Accepted:/Copyright
                // furniture). The row-aware (y,x) sort otherwise threads that rail
                // INTO the body paragraphs at matching Y-bands; segregating it so
                // each region reads contiguously matches a block-based extractor
                // and stops the rail-into-body interleave. Already used by the
                // md/html/structured paths; this wires the SAME ordering into the
                // plain-text path. Tightly gated (≥30 spans, narrow sidebar with
                // ≥2 furniture labels), so it is a no-op (None) on ordinary pages.
                spans = ordered;
            } else if let Some(gutter_x) = Self::prose_two_column_gutter(&spans)
                .or_else(|| Self::classifier_column_gutter(&spans))
            {
                // Genuine two-column prose (content-balance gated — forms /
                // TOC / tables / figures are rejected), OR a ragged
                // reference list / dense results body that the clean corridor
                // sweep and `is_multi_column_page` MISS but the per-column region
                // classifier confirms (`classifier_column_gutter`). Both read
                // column-major with band separation: full-width rows (titles,
                // mid-body section headings, footers — spans crossing the gutter)
                // are emitted at their vertical position, between the column runs
                // around them, so they are never split across the gutter
                // (§14.8.3). This branch is tried BEFORE the single-column
                // row-aware path so a 2-column reference page (which fails
                // `is_multi_column_page`) is reordered instead of interleaved;
                // both gutter detectors return None on single-column pages, which
                // then fall through to the row-aware branch unchanged.
                Self::reorder_column_major_with_bands(&mut spans, gutter_x);
                // NB: do NOT run reorder_same_line_runs here. The column emit
                // already orders each column by (y desc, x asc); a same-line
                // X-sort would re-merge vertically-adjacent lines whenever the
                // body leading (e.g. ~9pt) is tighter than same_line_threshold
                // (min_fs·1.2 ≈ 10.8pt), pulling a new left-margin reference
                // ahead of the previous reference's indented continuation
                // (bibliography interleave) or shattering wrapped hyphenated
                // lines in dense two-column bodies.
            } else if !Self::is_multi_column_page(&spans)
                || (!tables.is_empty() && Self::multicol_signal_is_tabular(&spans, &tables))
            {
                // Either a genuine single-column page, OR a single-column page
                // whose only multi-column geometric signal comes from a TABLE
                // (a data grid whose column-aligned cells trip
                // `is_multi_column_page`). In the latter case the genuine
                // two-column branches (topological / prose-gutter / classifier)
                // above all declined, so the page is NOT a two-column body; the
                // multi-column false positive is purely tabular. The correct
                // reading order is then the row-aware (y desc, x asc) band sort
                // — it linearises both the surrounding prose AND the table rows.
                // Without it the page keeps raw content-stream order, which on
                // these journal pages interleaves the table's column-major cell
                // stream INTO the prose paragraph (PMC8078162 §3.1). Gated on a
                // detected table whose region accounts for the multi-column
                // signal (`multicol_signal_is_tabular`), so genuine two-column
                // pages — which the column branches catch first, and which carry
                // no page-dominating table — are unaffected.
                spans.sort_by(|a, b| {
                    let cmp =
                        crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x);
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                    a.sequence.cmp(&b.sequence)
                });

                // Promote multi-row-spanning labels (sparse-column spans
                // vertically centred across several dense-column data rows)
                // to sort at the top of their row block.
                Self::reorder_rowspan_labels(&mut spans);

                // Restore intra-line reading order after the row-aware band sort.
                // Off-baseline glyphs (e.g. superscripts/subscripts) can land in
                // adjacent bands and be emitted out of X order; fix that per line.
                Self::reorder_same_line_runs(&mut spans);
            }

            // Re-append the lifted marginalia rail at the end of the body
            // reading order (Item 2 / M2). Done before the artifact retain so
            // any artifact-marked rail spans are still dropped.
            spans.extend(marginalia_trailing);

            // Drop content marked /Artifact (PDF Spec ISO 32000-1:2008
            // §14.8.2.2 — headers, footers, page numbers, decorations) —
            // unless the caller opted in via `options.include_artifacts`
            // (default true). Untagged-PDF running-header detection
            // runs at document level and feeds the same artifact_type flag.
            if !options.include_artifacts {
                spans.retain(|s| s.artifact_type.is_none());
            }

            // RTL correction
            Self::reverse_rtl_visual_order_runs(&mut spans);

            // Filter out invalid spans
            spans.retain(|s| {
                s.bbox.x.is_finite()
                    && s.bbox.y.is_finite()
                    && s.bbox.width.is_finite()
                    && s.bbox.height.is_finite()
                    && s.font_size.is_finite()
            });

            // Merge subscript/superscript spans into their base spans so that
            // tokens like "k1" and "k2" appear as single words rather than
            // as isolated fragments interleaved with other spans (pdfa_004).
            Self::merge_sub_superscript_spans(&mut spans);

            // Inline table insertion.
            //
            // Tables were previously rendered in a single block appended
            // at the end of the page text, after all flow spans. That
            // matches how `extract_text` historically worked but it means
            // tabular content appears far away from the prose that
            // surrounds it in reading order — on product data sheets
            // like ORAFOL 5900 the "Physical and Chemical Properties"
            // label/value rows showed up 20+ lines below the section
            // they belong to, which the reporter of #315 perceived as
            // the content being dropped entirely.
            //
            // Instead, maintain a sorted queue of tables keyed by their
            // top-Y (the larger Y coordinate of the table's bbox, per PDF
            // user-space conventions where Y grows upward). As we walk
            // the flow spans in row-aware reading order, whenever the
            // next span's top-Y falls below the top-Y of the queue's
            // leading table, we flush that table's rendered text at
            // that point, then continue. A final pass at the end emits
            // any tables whose top-Y is below all remaining spans (or
            // that have no flow spans at all).
            //
            // Tables are emitted at most once regardless of how many
            // spans sit above them, preserving existing behaviour
            // semantics while inlining the rendering at its spatial
            // reading-order position.
            let mut pending_tables: Vec<(f32, &crate::structure::table_extractor::Table)> = tables
                .iter()
                .filter_map(|t| t.bbox.map(|b| (b.y + b.height, t)))
                .collect();
            // Sort descending by top-Y so `pop()` returns the next table
            // to emit in reading order (larger Y first).
            pending_tables.sort_by(|(a, _), (b, _)| crate::utils::safe_float_cmp(*b, *a));

            let flush_table =
                |text: &mut String, table: &crate::structure::table_extractor::Table| {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push('\n');
                    text.push_str(&table.render_text());
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                };

            let mut text = String::with_capacity(spans.len() * 20);
            let mut prev_span: Option<TextSpan> = None;

            for span in &spans {
                // Flush any tables that sit above this span in PDF
                // reading order (their top-Y is greater than or equal
                // to the span's top-Y, meaning they should appear first).
                while let Some(&(table_top_y, table)) = pending_tables.last() {
                    let span_top_y = span.bbox.y + span.bbox.height;
                    if table_top_y >= span_top_y {
                        flush_table(&mut text, table);
                        pending_tables.pop();
                        // Reset prev_span so the flow-text glue logic
                        // doesn't try to stitch the table's rendered
                        // block together with the next flow span.
                        prev_span = None;
                    } else {
                        break;
                    }
                }

                if let Some(prev) = &prev_span {
                    let prev_end_x = prev.bbox.x + prev.bbox.width;
                    let span_end_x = span.bbox.x + span.bbox.width;
                    // Containment check: skip a span only if it is geometrically
                    // contained within the previous span AND has identical text.
                    // Without the text comparison, distinct lines that happen to
                    // overlap spatially (e.g., due to small Tm-scaled offsets)
                    // would be silently dropped.
                    let y_same = (prev.bbox.y - span.bbox.y).abs() < 2.0;
                    if y_same
                        && span.bbox.x >= prev.bbox.x - 0.5
                        && span_end_x <= prev_end_x + 0.5
                        && span.text == prev.text
                    {
                        continue;
                    }

                    let y_diff = (prev.bbox.y - span.bbox.y).abs();
                    let gap = span.bbox.x - prev_end_x;
                    let delta_x = span.bbox.x - prev.bbox.x;

                    // Korean mid-eojeol soft wrap (SEG-KO): keep a Hangul word whole
                    // when it wrapped mid-syllable. The wrap surfaces either as a
                    // y-line break OR (when the two halves share a baseline band) as a
                    // large backward X jump, so it is gated at each break site below.
                    let hangul_midword_wrap = Self::hangul_midword_line_wrap(&text, prev, span);
                    if y_diff > Self::same_line_threshold(prev, span) {
                        let font_size = prev.font_size.max(span.font_size).max(10.0);
                        let line_height = font_size * 1.2;
                        let num_breaks = (y_diff / line_height).round() as usize;
                        if !(hangul_midword_wrap && num_breaks == 1) {
                            for _ in 0..num_breaks.clamp(1, 3) {
                                text.push('\n');
                            }
                        }
                    } else if gap < -1.0 {
                        let fs = span.font_size.max(prev.font_size).max(6.0);
                        if gap < -(fs * 20.0) {
                            if !hangul_midword_wrap && !text.ends_with('\n') {
                                text.push('\n');
                            }
                        } else if delta_x < -fs * 3.0 {
                            // Same visual line, but the current span starts well to the LEFT of the
                            // previous span's start — i.e., the upstream ordering is non-monotonic in X.
                            // This commonly occurs with multi-column layouts or XY-cut artifacts where
                            // spans from different visual rows fall within the same Y tolerance band
                            // (see `same_line_threshold`).
                            //
                            // Without inserting a separator, these spans would be concatenated
                            // (e.g. `instancesinstancesinstances` from adjacent table headers).
                            // Treat this backward X jump as a logical break and emit a newline.
                            if !text.ends_with('\n') {
                                text.push('\n');
                            }
                        } else if gap < -fs * 3.0 && y_diff > fs * 0.5 && delta_x <= fs * 0.5 {
                            // Soft-wrapped next line (carriage return). Three
                            // signals must coincide so inline math/super-scripts
                            // and chart labels are NOT mistaken for a wrap:
                            //   • `gap < -fs*3` — the previous span ended far to
                            //     the RIGHT of where this one starts, i.e. the
                            //     line filled and wrapped (adjacent math glyphs
                            //     have a near-zero gap, so they are excluded);
                            //   • `y_diff > fs*0.5` — a real baseline drop (a
                            //     super/sub-script shift is smaller);
                            //   • `delta_x <= fs*0.5` — it returned to ~the line's
                            //     left margin.
                            // Its leading sits UNDER `same_line_threshold`
                            // (single-spaced body at ~1.0 em vs the 1.2 em
                            // threshold) so the y-newline branch above never
                            // fired, leaving the line-final and line-initial words
                            // glued together. A line break encodes no space (ISO
                            // 32000 §9.4.2 — positioning is geometric); synthesize
                            // one as a newline, which the downstream join /
                            // `normalize_text` collapses to a space and
                            // de-hyphenates.
                            if !hangul_midword_wrap && !text.ends_with('\n') {
                                text.push('\n');
                            }
                        } else if y_diff > 1.0
                            && delta_x <= 0.5
                            && gap < -fs
                            && !prev.rtl_draw_logical
                            && !span.rtl_draw_logical
                            && !crate::text::bidi::looks_rtl(&prev.text)
                            && !crate::text::bidi::looks_rtl(&span.text)
                        {
                            // Backtracking span with a real baseline offset,
                            // under the soft-wrap thresholds above: displayed
                            // math draws a fraction's denominator AFTER the
                            // relation sign that follows the numerator, so
                            // the next span starts at-or-left-of the previous
                            // span's ORIGIN with an overlap far beyond
                            // kerning (a denominator sits ~2 em back at a
                            // ~0.3 em baseline offset; stacked column cells
                            // land at delta_x ≈ 0 a row-pitch down). Bare
                            // concatenation fused these into tokens like
                            // "=dt" — break the line instead. Same-baseline
                            // kerned runs (y_diff ≈ 0) and forward-advancing
                            // subscripts (gap > -1 em) never reach here.
                            if !hangul_midword_wrap && !text.ends_with('\n') {
                                text.push('\n');
                            }
                        } else if prev.font_name != span.font_name
                            && span_end_x > prev_end_x + 0.5
                            && !text.ends_with(' ')
                            && !text.ends_with('\n')
                        {
                            text.push(' ');
                        } else if delta_x > fs * 1.5
                            && !text.ends_with(' ')
                            && !text.ends_with('\n')
                            && !Self::is_reliable_kerning_overlap(prev, span, gap)
                        {
                            // Inflated-width overlap recovery.
                            // A negative raw gap here usually comes from a
                            // font whose `/Widths` array is missing
                            // `FontInfo::new` fell back to the 550/1000-em
                            // constant, which over-reports each glyph's
                            // advance and drags `prev_end_x` past the real
                            // start of the next span. When the two spans'
                            // actual origins (`delta_x`) are separated by
                            // more than 1.5 em, they cannot both belong to
                            // the same word — the overlap is a width-table
                            // artifact, not real kerning — so insert a
                            // space to preserve the word boundary. This
                            // rescues cases like "STATION" + "FREEDOM"
                            // "UTILIZATION" + "CONFERENCE" in the NASA
                            // Apollo report header where raw gaps of
                            // -1.75 pt and -12.75 pt sit alongside
                            // delta_x values of 56 pt and 78 pt.
                            text.push(' ');
                        }
                    } else if y_diff > 2.0
                        && gap > FORWARD_GAP_K * prev.font_size.max(span.font_size).max(1.0)
                    {
                        // Forward-gap guard: pairs newly admitted to same-line
                        // handling by the widened threshold get a column/field-
                        // boundary check against FORWARD_GAP_K * max(fs).
                        // the constant's doc comment for calibration notes.
                        if !text.ends_with('\n') {
                            text.push('\n');
                        }
                    } else if prev.font_name != span.font_name
                        && gap > 0.5
                        && gap < prev.font_size.max(span.font_size).max(6.0) * 3.0
                        && !text.ends_with(' ')
                        && !text.ends_with('\n')
                    {
                        // Same-line font transition with a meaningful
                        // positive gap. Cross-font runs that survive the
                        // upstream `cross_font_word_glue` merge (i.e.
                        // both sides are multi-char) are word boundaries
                        // even when the gap is too small for the generic
                        // `should_insert_space` threshold (0.15 × fs) —
                        // e.g. roman → italic transitions in academic
                        // paper headers sit at ~2.7 pt at 10.9 pt body.
                        text.push(' ');
                    } else if Self::should_insert_space(prev, span) {
                        text.push(' ');
                    } else {
                        let fs = span.font_size.max(prev.font_size).max(6.0);
                        if gap > fs * 3.0 {
                            text.push('\n');
                        }
                    }
                }

                Self::push_span_text(&mut text, span);
                prev_span = Some(span.clone());
            }

            // Drain any tables that sit below all flow spans (or the
            // page had no flow spans at all). Without this final
            // pass they would be silently dropped now that the
            // end-of-page `for table in tables` block has been
            // removed.
            while let Some((_, table)) = pending_tables.pop() {
                flush_table(&mut text, table);
            }
            text
        };

        // Annotation text is already included via annotation_content_spans() in
        // extract_spans() — do NOT call append_non_widget_annotation_text() here,
        // as that would emit every annotation a second time.

        // Filter leaked PDF metadata
        let final_text = Self::filter_leaked_metadata(&text);

        // Normalize Kangxi Radicals
        let final_text = Self::normalize_kangxi_radicals(&final_text);

        // Normalize Arabic Presentation Forms
        let final_text = Self::normalize_arabic_presentation_forms(&final_text);

        let cleaned_text = final_text;

        // For tagged PDFs, the structure-tree traversal at line 4306 already
        // captures all table-cell content via MCIDs. Appending tables here
        // would double-emit that content (structure-tree text + table render),
        // dropping precision. For untagged PDFs, tables are inlined via
        // pending_tables above, so this block is never reached (cached_tree
        // is None → condition would be false). The block is removed.

        // #317 UTF-8 mojibake repair: a run of Latin-1 Supplement chars
        // whose raw bytes form valid UTF-8 decoding to non-Latin-1 code
        // points is almost certainly a double-encoded non-Latin string
        // (Cyrillic, Greek, CJK, Arabic, Hebrew, …) that surfaced
        // because the producing font had no ToUnicode CMap and the
        // /Differences / AGL lookup returned the UTF-8 byte sequence
        // re-interpreted as Latin-1. Re-decode those runs in place.
        let cleaned_text = Self::repair_utf8_mojibake(&cleaned_text);

        // Optionally expand Latin ligature characters to their component letters.
        let cleaned_text = if options.expand_ligatures {
            cleaned_text
                .replace('\u{FB00}', "ff")
                .replace('\u{FB01}', "fi")
                .replace('\u{FB02}', "fl")
                .replace('\u{FB03}', "ffi")
                .replace('\u{FB04}', "ffl")
                .replace(['\u{FB05}', '\u{FB06}'], "st")
        } else {
            cleaned_text
        };

        // Drop stray spaces a producer inserted between a CJK ideograph and an
        // embedded ASCII number (e.g. "公元前 1000 年" → "公元前1000年").
        let cleaned_text = crate::extractors::text::strip_cjk_digit_boundary_spaces(&cleaned_text);

        // Drop a space the word-break heuristic injected inside a prime-notation
        // number (e.g. "0′′ .28" / "0′′. 28" → "0′′.28").
        let cleaned_text =
            crate::extractors::text::strip_prime_decimal_boundary_spaces(&cleaned_text);

        Ok(cleaned_text)
    }

    /// Walk `input` and repair runs of Latin-1 Supplement characters
    /// whose raw byte values form a valid UTF-8 sequence whose decoded
    /// codepoints include at least one non-Latin-1 character.
    ///
    /// This undoes the most common shape of "Cyrillic served as
    /// Latin-1" mojibake that surfaces on PDFs whose fonts have no
    /// ToUnicode CMap. The decoded-codepoint gate (≥ U+0100 somewhere
    /// in the decoded run) ensures genuine Latin-1 content like
    /// "Résumé" — which also decodes as valid UTF-8 but stays entirely
    /// within U+0000..U+00FF — is left alone.
    fn repair_utf8_mojibake(input: &str) -> String {
        // Fast-path: if the string contains no Latin-1 Supplement codepoints
        // (U+0080..=U+00FF), there is nothing to repair. This avoids the
        // O(n) `Vec<char>` allocation on every ASCII-only page.
        if !input.chars().any(|c| matches!(c as u32, 0x80..=0xFF)) {
            return input.to_string();
        }
        let mut out = String::with_capacity(input.len());
        let chars: Vec<char> = input.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let mut j = i;
            while j < chars.len() {
                let cc = chars[j] as u32;
                if (0x80..=0xFF).contains(&cc) {
                    j += 1;
                } else {
                    break;
                }
            }
            if j - i >= 2 {
                let bytes: Vec<u8> = chars[i..j].iter().map(|&c| c as u8).collect();
                if let Ok(decoded) = std::str::from_utf8(&bytes) {
                    if decoded.chars().any(|c| c as u32 > 0xFF) {
                        out.push_str(decoded);
                        i = j;
                        continue;
                    }
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    /// Extract text from all pages of the document.
    ///
    /// Concatenates text from every page, separated by form feed characters (`\x0c`).
    /// This is a convenience method equivalent to calling `extract_text()` for each page.
    ///
    /// # Returns
    ///
    /// The combined text from all pages.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("paper.pdf")?;
    /// let all_text = doc.extract_all_text()?;
    /// println!("Full document: {} chars", all_text.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_all_text(&self) -> Result<String> {
        let num_pages = self.page_count()?;
        let mut result = String::new();

        for i in 0..num_pages {
            if i > 0 {
                result.push('\x0c'); // Form feed page separator
            }
            match self.extract_text(i) {
                Ok(text) => result.push_str(&text),
                Err(e) => {
                    log::warn!("Failed to extract text from page {}: {}", i, e);
                }
            }
        }

        Ok(result)
    }

    /// Filter leaked PDF internal metadata from extracted text.
    ///
    /// Some PDFs embed inline ColorSpace definitions (CalRGB, CalGray, Lab) that
    /// get parsed as text content. This removes known metadata patterns like
    /// "WhitePoint [ ... ]", "BlackPoint [ ... ]", "Gamma [ ... ]", "Matrix [ ... ]".
    fn filter_leaked_metadata(text: &str) -> String {
        // Known PDF metadata keys that should never appear in extracted text.
        // These come from CalRGB/CalGray/Lab color space dictionaries.
        const METADATA_PATTERNS: &[&str] = &[
            "WhitePoint",
            "BlackPoint",
            "Gamma",
            "Matrix",
            "CalRGB",
            "CalGray",
        ];

        // Quick check: if none of the patterns appear, return as-is
        if !METADATA_PATTERNS.iter().any(|p| text.contains(p)) {
            return text.to_string();
        }

        // Filter line-by-line: remove lines that look like PDF metadata
        let mut result = String::with_capacity(text.len());
        for line in text.lines() {
            let trimmed = line.trim();
            // Skip lines matching "MetadataKey [ ... ]" or "MetadataKey [ ... ] ..."
            let is_metadata = METADATA_PATTERNS.iter().any(|pattern| {
                if let Some(rest) = trimmed.strip_prefix(pattern) {
                    // Must be followed by whitespace and bracket, or end of line
                    let rest = rest.trim_start();
                    rest.is_empty()
                        || rest.starts_with('[')
                        || rest.starts_with('/')
                        || rest.starts_with('<')
                } else {
                    false
                }
            });

            if !is_metadata {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(line);
            }
        }

        result
    }

    /// Normalize Kangxi Radical characters to CJK Unified Ideographs.
    ///
    /// Some PDF fonts/CMaps emit Kangxi Radicals (U+2F00–U+2FD5) or CJK Radicals
    /// Supplement (U+2E80–U+2EFF) instead of the standard CJK Unified Ideographs.
    /// While visually similar, these are different Unicode codepoints and will break
    /// text search, string matching, and NLP pipelines.
    fn normalize_kangxi_radicals(text: &str) -> String {
        // Quick check: if no characters in the Kangxi/Supplement range, return as-is
        if !text.chars().any(|c| {
            let cp = c as u32;
            (0x2E80..=0x2EFF).contains(&cp) || (0x2F00..=0x2FD5).contains(&cp)
        }) {
            return text.to_string();
        }

        text.chars()
            .map(|c| crate::text::kangxi::kangxi_to_unified(c).unwrap_or(c))
            .collect()
    }

    /// Reverse visual-order RTL character runs to logical reading order.
    ///
    /// Some PDFs position Arabic/Hebrew characters individually left-to-right
    /// (visual order). For correct text extraction, runs of single-character
    /// RTL spans on the same line are collected, reversed, and merged into
    /// a single span to produce correct logical reading order.
    fn reverse_rtl_visual_order_runs(spans: &mut Vec<TextSpan>) {
        use crate::text::rtl_detector::is_rtl_text;

        // Pass 0: reverse visual-order characters inside a single span
        // when the producer clearly emitted pre-shaped Arabic.
        //
        // Some PDFs (e.g. `ArabicCIDTrueType.pdf` in the pdfjs regression
        // corpus) emit Arabic with an entire line as a single Tj-produced
        // span whose `text` is stored in *visual* order — rightmost
        // rendered glyph first. That matches what the content stream
        // literally drew on the page, but downstream consumers expect
        // reading-order (logical) text.
        //
        // The gate for reversal is the presence of **Arabic Presentation
        // Forms A or B** (U+FB50-U+FDFF, U+FE70-U+FEFF). Those code points
        // only appear when the PDF producer has explicitly pre-shaped the
        // glyphs, and producers that pre-shape almost universally also
        // store them in visual order because that's the order the content
        // stream draws them. Plain base-Arabic text (U+0600-U+06FF) is
        // left alone because those files are usually already in logical
        // order — the PDF viewer applies shaping and bidi reordering at
        // render time, so reversing would produce a wrong result.
        //
        // We still require at least 4 characters and >50 % non-whitespace
        // RTL ratio so that punctuation or stray markers adjacent to
        // Arabic do not trigger a reversal.
        //
        // Pass 1 below handles the other common shape where each Arabic
        // character is emitted as its own short span and the reversal is
        // a span-granularity concern. The two passes are independent:
        // a span either fires Pass 0 (pre-shaped, reverse in place) or
        // Pass 1 (per-glyph spans, reverse span order), never both.
        //
        // This is separate from `normalize_arabic_presentation_forms`,
        // which runs later on the assembled output string and unshapes
        // contextual glyphs back to their base Unicode letters.
        for span in spans.iter_mut() {
            let mut total = 0usize;
            let mut rtl_count = 0usize;
            let mut has_presentation_form = false;
            for c in span.text.chars() {
                if c.is_whitespace() {
                    continue;
                }
                total += 1;
                let cp = c as u32;
                if is_rtl_text(cp) {
                    rtl_count += 1;
                }
                if (0xFB50..=0xFDFF).contains(&cp) || (0xFE70..=0xFEFF).contains(&cp) {
                    has_presentation_form = true;
                }
            }
            // #557: Pass 0 only applies to a *whole-line* visual-order span —
            // one span holding several words separated by internal whitespace,
            // in the order the content stream drew them (rightmost first). When
            // the extractor instead emits one span PER WORD (the common
            // CID-TrueType case, e.g. ArabicCIDTrueType.pdf), each word's
            // characters are already in logical order, so char-reversing them
            // here corrupts them. Their right-to-left *word* order is fixed
            // separately by the span-run reversal pass below. Gate on internal
            // whitespace so per-word logical spans are left untouched.
            let has_internal_whitespace = span.text.trim().chars().any(|c| c.is_whitespace());
            if has_presentation_form
                && has_internal_whitespace
                && total >= 4
                && rtl_count * 2 > total
            {
                let reversed: String = span.text.chars().rev().collect();
                span.text = reversed;
            }
        }

        // #557 Pass 0.5: per-word RTL span ORDER. The row-aware sort placed
        // spans left-to-right (x ascending), but a right-to-left script reads
        // the words in the opposite direction. For each maximal run of
        // consecutive same-line spans that is purely RTL (every non-space span
        // holds RTL letters and no Latin letters), reverse the run's order so
        // the words come out in logical reading order. Each word's characters
        // are left as-is (they are already logical — see Pass 0's gate).
        let is_space = |s: &TextSpan| s.text.trim().is_empty();
        let is_rtl_word = |s: &TextSpan| {
            let mut has_rtl = false;
            for c in s.text.chars() {
                if c.is_ascii_alphabetic() {
                    return false; // Latin letter → not a pure-RTL word
                }
                if is_rtl_text(c as u32) {
                    has_rtl = true;
                }
            }
            has_rtl
        };
        let mut i = 0;
        while i < spans.len() {
            if !is_rtl_word(&spans[i]) {
                i += 1;
                continue;
            }
            let y = spans[i].bbox.y;
            let start = i;
            let mut end = i + 1;
            while end < spans.len()
                && (spans[end].bbox.y - y).abs() < 2.0
                && (is_rtl_word(&spans[end]) || is_space(&spans[end]))
            {
                end += 1;
            }
            // Trim trailing space spans so separators stay between words.
            let mut last = end;
            while last > start + 1 && is_space(&spans[last - 1]) {
                last -= 1;
            }
            if last - start >= 2 {
                spans[start..last].reverse();
            }
            i = end;
        }

        if spans.len() < 4 {
            return;
        }

        // Iterate forward; drain consumed runs so subsequent indices stay valid
        let mut i = 0;
        while i < spans.len() {
            // Check if this span starts an RTL single-char run
            let is_short_rtl = spans[i].text.chars().count() <= 2
                && spans[i].text.chars().any(|c| is_rtl_text(c as u32));

            if !is_short_rtl {
                i += 1;
                continue;
            }

            // Find the end of this RTL run (consecutive short spans on same line)
            let run_start = i;
            let y = spans[i].bbox.y;
            let mut j = i + 1;
            while j < spans.len() {
                let y_same = (spans[j].bbox.y - y).abs() < 2.0;
                let is_short = spans[j].text.chars().count() <= 2;
                let has_rtl_or_space = spans[j]
                    .text
                    .chars()
                    .all(|c| is_rtl_text(c as u32) || c == ' ');
                if y_same && is_short && has_rtl_or_space {
                    j += 1;
                } else {
                    break;
                }
            }
            let run_end = j;
            let run_len = run_end - run_start;

            // Only process runs of 4+ spans (avoid false positives)
            if run_len >= 4 {
                // Collect span texts in reverse order (visual LTR → logical RTL).
                // Preserve space spans as word separators.
                let mut reversed_text = String::new();
                for span in spans[run_start..run_end].iter().rev() {
                    reversed_text.push_str(&span.text);
                }

                // Merge into first span, expand bbox to cover entire run
                let last_span = &spans[run_end - 1];
                let new_width = (last_span.bbox.x + last_span.bbox.width) - spans[run_start].bbox.x;
                spans[run_start].text = reversed_text;
                spans[run_start].bbox.width = new_width;

                // Remove the rest of the run
                spans.drain(run_start + 1..run_end);

                i = run_start + 1;
            } else {
                i = run_end;
            }
        }
    }

    /// Normalize Arabic Presentation Forms to base Unicode characters.
    ///
    /// Arabic PDFs often use presentation forms (U+FE70-U+FEFF for Forms-B,
    /// U+FB50-U+FDFF for Forms-A) which represent contextual glyph shapes.
    /// For text extraction, these should be normalized to base characters.
    fn normalize_arabic_presentation_forms(text: &str) -> String {
        // Quick check: skip if no Arabic presentation form characters
        if !text.chars().any(|c| {
            let cp = c as u32;
            (0xFB50..=0xFDFF).contains(&cp) || (0xFE70..=0xFEFF).contains(&cp)
        }) {
            return text.to_string();
        }

        text.chars()
            .map(|c| {
                let cp = c as u32;
                // Arabic Presentation Forms-B (U+FE70-U+FEFF): contextual forms
                // Each base letter has isolated/final/initial/medial forms
                let base = match cp {
                    // Hamza forms
                    0xFE80 => 0x0621,
                    // Alef with Madda
                    0xFE81 | 0xFE82 => 0x0622,
                    // Alef with Hamza Above
                    0xFE83 | 0xFE84 => 0x0623,
                    // Waw with Hamza
                    0xFE85 | 0xFE86 => 0x0624,
                    // Alef with Hamza Below
                    0xFE87 | 0xFE88 => 0x0625,
                    // Yeh with Hamza
                    0xFE89..=0xFE8C => 0x0626,
                    // Alef
                    0xFE8D | 0xFE8E => 0x0627,
                    // Beh
                    0xFE8F..=0xFE92 => 0x0628,
                    // Teh Marbuta
                    0xFE93 | 0xFE94 => 0x0629,
                    // Teh
                    0xFE95..=0xFE98 => 0x062A,
                    // Theh
                    0xFE99..=0xFE9C => 0x062B,
                    // Jeem
                    0xFE9D..=0xFEA0 => 0x062C,
                    // Hah
                    0xFEA1..=0xFEA4 => 0x062D,
                    // Khah
                    0xFEA5..=0xFEA8 => 0x062E,
                    // Dal
                    0xFEA9 | 0xFEAA => 0x062F,
                    // Thal
                    0xFEAB | 0xFEAC => 0x0630,
                    // Reh
                    0xFEAD | 0xFEAE => 0x0631,
                    // Zain
                    0xFEAF | 0xFEB0 => 0x0632,
                    // Seen
                    0xFEB1..=0xFEB4 => 0x0633,
                    // Sheen
                    0xFEB5..=0xFEB8 => 0x0634,
                    // Sad
                    0xFEB9..=0xFEBC => 0x0635,
                    // Dad
                    0xFEBD..=0xFEC0 => 0x0636,
                    // Tah
                    0xFEC1..=0xFEC4 => 0x0637,
                    // Zah
                    0xFEC5..=0xFEC8 => 0x0638,
                    // Ain
                    0xFEC9..=0xFECC => 0x0639,
                    // Ghain
                    0xFECD..=0xFED0 => 0x063A,
                    // Feh
                    0xFED1..=0xFED4 => 0x0641,
                    // Qaf
                    0xFED5..=0xFED8 => 0x0642,
                    // Kaf
                    0xFED9..=0xFEDC => 0x0643,
                    // Lam
                    0xFEDD..=0xFEE0 => 0x0644,
                    // Meem
                    0xFEE1..=0xFEE4 => 0x0645,
                    // Noon
                    0xFEE5..=0xFEE8 => 0x0646,
                    // Heh
                    0xFEE9..=0xFEEC => 0x0647,
                    // Waw
                    0xFEED | 0xFEEE => 0x0648,
                    // Alef Maksura
                    0xFEEF | 0xFEF0 => 0x0649,
                    // Yeh
                    0xFEF1..=0xFEF4 => 0x064A,
                    // Lam-Alef ligatures → expand to two characters
                    0xFEF5 | 0xFEF6 => {
                        // Lam + Alef with Madda
                        return '\u{0644}'; // Just return Lam; Alef is separate
                    }
                    0xFEF7 | 0xFEF8 => {
                        return '\u{0644}'; // Lam + Alef with Hamza Above
                    }
                    0xFEF9 | 0xFEFA => {
                        return '\u{0644}'; // Lam + Alef with Hamza Below
                    }
                    0xFEFB | 0xFEFC => {
                        return '\u{0644}'; // Lam + Alef
                    }
                    // Tatweel (kashida)
                    0xFE70 => 0x064B, // Fathatan isolated
                    0xFE71 => 0x064B, // Tatweel + Fathatan
                    0xFE72 => 0x064C, // Dammatan isolated
                    0xFE74 => 0x064D, // Kasratan isolated
                    0xFE76 => 0x064E, // Fatha isolated
                    0xFE77 => 0x064E, // Fatha medial
                    0xFE78 => 0x064F, // Damma isolated
                    0xFE79 => 0x064F, // Damma medial
                    0xFE7A => 0x0650, // Kasra isolated
                    0xFE7B => 0x0650, // Kasra medial
                    0xFE7C => 0x0651, // Shadda isolated
                    0xFE7D => 0x0651, // Shadda medial
                    0xFE7E => 0x0652, // Sukun isolated
                    0xFE7F => 0x0652, // Sukun medial
                    _ => cp,          // Pass through unchanged
                };
                char::from_u32(base).unwrap_or(c)
            })
            .collect()
    }

    /// Returns the Y tolerance (in points) for treating two spans as
    /// belonging to the same visual line during text assembly.
    ///
    /// The threshold scales with the larger font size so mixed-size runs
    /// (for example superscripts and subscripts) are not split by a fixed
    /// absolute tolerance.
    fn same_line_threshold(prev: &TextSpan, current: &TextSpan) -> f32 {
        let max_fs = prev.font_size.max(current.font_size).max(1.0);
        let min_fs = prev.font_size.min(current.font_size).max(1.0);
        // Continuous formula — avoids the step discontinuity at the 4×
        // ratio boundary. Examples:
        //   same-size 12 pt body: max(12×1.2, 12×0.3) = 14.4 pt ← 1.2× leading
        //   heading+body 24+10 pt: max(10×1.2, 24×0.3) = 12.0 pt ← keeps para break
        //   superscript 12+6 pt: max(6×1.2, 12×0.3) = 7.2 pt ← same line
        // Prior formula was max_fs×0.5 for normal ratios; new formula uses 1.2× of the
        // smaller font, which is wider and reduces false newlines for normal leading.
        // Formula: max(min_fs * 1.2, max_fs * 0.3)
        (min_fs * 1.2).max(max_fs * 0.3)
    }

    /// True when a line break falls *inside* a Hangul word (eojeol) that wrapped
    /// mid-syllable — Korean breaks anywhere, not only at word boundaries, so a
    /// mid-eojeol wrap carries no separator in the source and the two halves
    /// must rejoin with nothing ("집고양" ⏎ "이의" → "집고양이의"). An
    /// eojeol-BOUNDARY wrap keeps its explicit inter-eojeol space, so `text`
    /// ends with ' ' and this returns false (the break still separates).
    /// Scoped to Hangul (not Chinese/Japanese) to avoid the CJK
    /// line-break-collapse regressions seen in v0.3.62.
    fn hangul_midword_line_wrap(text: &str, prev: &TextSpan, span: &TextSpan) -> bool {
        let is_hangul = |c: char| (0xAC00..=0xD7AF).contains(&(c as u32));
        !text.ends_with(' ')
            && prev.text.chars().next_back().is_some_and(is_hangul)
            && span.text.chars().next().is_some_and(is_hangul)
    }

    /// Returns `true` if `inner` is contained within `outer`,
    /// allowing `eps` points of floating-point slack on all four
    /// edges. Used at the table-retain sites to absorb ~0.02pt drift
    /// in span right-edges relative to table bboxes computed from
    /// min/max reductions over many cell edges.
    fn contains_rect_with_tolerance(
        outer: &crate::geometry::Rect,
        inner: &crate::geometry::Rect,
        eps: f32,
    ) -> bool {
        inner.left() >= outer.left() - eps
            && inner.right() <= outer.right() + eps
            && inner.top() >= outer.top() - eps
            && inner.bottom() <= outer.bottom() + eps
    }

    /// Returns `true` if a tentative left-to-right X-ordering of `run`
    /// contains a horizontal gap exceeding
    /// `SAME_LINE_REORDER_MAX_GAP_FACTOR * max(font_size)` between any
    /// two consecutive spans. Used by [`reorder_same_line_runs`] to
    /// reject candidate runs that are vertically close but horizontally
    /// disjoint (e.g. tightly-set footer/header rows split across the
    /// page).
    ///
    /// The slice is not mutated; the X-order is computed on a local
    /// copy of `(left_x, right_x, font_size)` triples.
    fn run_has_large_x_gap(run: &[TextSpan]) -> bool {
        if run.len() < 2 {
            return false;
        }

        let mut edges: Vec<(f32, f32, f32)> = run
            .iter()
            .map(|s| (s.bbox.x, s.bbox.x + s.bbox.width, s.font_size))
            .collect();

        edges.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));

        for pair in edges.windows(2) {
            let prev = pair[0];
            let cur = pair[1];

            let gap = cur.0 - prev.1;
            if gap <= 0.0 {
                continue;
            }

            let max_fs = prev.2.max(cur.2).max(1.0);
            if gap > SAME_LINE_REORDER_MAX_GAP_FACTOR * max_fs {
                return true;
            }
        }

        false
    }

    /// True when a candidate run contains spans whose X-extents OVERLAP — the
    /// signature of two (or more) distinct text lines that the same-line Y
    /// tolerance merged into one band, NOT a single line. A real line lays its
    /// spans out left-to-right with non-overlapping advances; only stacked lines
    /// (leading just under `same_line_threshold`, e.g. a two-line title or a
    /// running head sitting above the line below it) put two spans at the same
    /// horizontal position. X-sorting such a band interleaves the two lines word
    /// by word, so the caller must leave it in row order instead. Mirrors
    /// [`run_has_large_x_gap`] for the opposite defect.
    fn run_has_x_overlap(run: &[TextSpan]) -> bool {
        if run.len() < 2 {
            return false;
        }

        let mut edges: Vec<(f32, f32, f32)> = run
            .iter()
            .map(|s| (s.bbox.x, s.bbox.x + s.bbox.width, s.font_size))
            .collect();

        edges.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));

        for pair in edges.windows(2) {
            let prev = pair[0];
            let cur = pair[1];

            // prev.right - cur.left > 0 ⇒ the next span starts before the previous
            // one ends (horizontal overlap). Half an em of overlap is well beyond
            // kerning/italic side-bearing and only happens across stacked lines.
            let overlap = prev.1 - cur.0;
            let max_fs = prev.2.max(cur.2).max(1.0);
            if overlap > 0.5 * max_fs {
                return true;
            }
        }

        false
    }

    /// True when a run is structurally two-or-more stacked text LINES: it has at
    /// least two distinct Y levels that EACH carry at least two spans. This
    /// separates a real two-line title / running-head block (many words on each
    /// of two baselines) — where de-interleaving is correct — from a single span
    /// that merely overlaps a line in X (a drop cap, a `©`/`c` mark, a lone
    /// super-script), where the existing X-sort already does the right thing and
    /// reordering by Y would misplace the stray glyph.
    fn run_is_stacked_lines(run: &[TextSpan]) -> bool {
        if run.len() < 4 {
            return false; // need ≥2 lines × ≥2 spans
        }
        let mut rows: Vec<(f32, f32)> = run.iter().map(|s| (s.bbox.y, s.font_size)).collect();
        rows.sort_by(|a, b| crate::utils::safe_float_cmp(b.0, a.0));

        let mut multi_rows = 0usize;
        let mut anchor_y = f32::NAN;
        let mut count = 0usize;
        for (y, fs) in rows {
            if anchor_y.is_nan() || (anchor_y - y).abs() <= 0.5 * fs.max(1.0) {
                if anchor_y.is_nan() {
                    anchor_y = y;
                }
                count += 1;
            } else {
                if count >= 2 {
                    multi_rows += 1;
                }
                anchor_y = y;
                count = 1;
            }
        }
        if count >= 2 {
            multi_rows += 1;
        }
        multi_rows >= 2
    }

    /// Re-sort same-line spans by X after row-aware band sorting.
    ///
    /// Row-aware sorting can place off-baseline glyphs such as superscripts or
    /// subscripts in adjacent Y bands before their base glyphs. This helper finds
    /// candidate runs with the existing same-line threshold, then tentatively views
    /// each candidate in X order. If that tentative X order contains a large gap,
    /// the candidate is treated as disjoint footer/header/field content and is
    /// left in the existing row-aware order.
    ///
    /// At the slice level no spans are merged or dropped; successful candidates are
    /// only permuted. Downstream text assembly may then emit the reordered spans
    /// into one visual line, which is the user-observable effect.
    fn reorder_same_line_runs(spans: &mut [TextSpan]) {
        let mut i = 0;

        while i < spans.len() {
            let mut j = i + 1;

            while j < spans.len() {
                let anchor = &spans[i];
                let prev = &spans[j - 1];
                let cur = &spans[j];

                let to_prev = (cur.bbox.y - prev.bbox.y).abs();
                let to_anchor = (cur.bbox.y - anchor.bbox.y).abs();

                let tol_prev = Self::same_line_threshold(prev, cur);
                let tol_anchor = Self::same_line_threshold(anchor, cur);

                if to_prev > tol_prev || to_anchor > tol_anchor {
                    break;
                }

                j += 1;
            }

            if j - i > 1 {
                if Self::run_has_large_x_gap(&spans[i..j]) {
                    // Candidate spans are vertically close but not horizontally
                    // contiguous (disjoint header/footer columns). Do not X-sort
                    // them into a fake line; preserve the row-aware order.
                    i = j;
                    continue;
                }

                if Self::run_has_x_overlap(&spans[i..j]) && Self::run_is_stacked_lines(&spans[i..j])
                {
                    // Spans OVERLAP horizontally AND form ≥2 lines of ≥2 spans each:
                    // two stacked lines the Y tolerance merged into one band (a
                    // two-line title, a running head above the line below it). A
                    // flat X-sort interleaves them word by word. De-interleave by
                    // ordering on (Y-descending, then X) so each real line stays
                    // contiguous and in reading order. The stacked-lines gate keeps
                    // a lone overlapping glyph (drop cap, `©`, super-script) on the
                    // normal X-sort path below.
                    spans[i..j].sort_by(|a, b| {
                        crate::utils::safe_float_cmp(b.bbox.y, a.bbox.y)
                            .then(crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x))
                            .then(a.sequence.cmp(&b.sequence))
                    });
                    i = j;
                    continue;
                }

                spans[i..j].sort_by(|a, b| {
                    let cmp = crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x);
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                    a.sequence.cmp(&b.sequence)
                });
            }

            i = j;
        }
    }

    /// Distinguish a genuine tight-kerning overlap of a single word drawn as
    /// two same-font runs ("PLANAL"+"TINA") from an inflated-width artifact.
    ///
    /// A font with no `/Widths` array falls back to a uniform 550/1000-em
    /// advance for every glyph, which over-reports each glyph's width and drags
    /// the previous span's right edge past where the next span really starts —
    /// a fake overlap that the assembler must break with a space (the NASA
    /// "STATION"+"FREEDOM" header case). A genuine kerning overlap, by
    /// contrast, has real per-glyph metrics that VARY across the run, a modest
    /// overlap (well under one em), the same font on both sides, and word
    /// characters at the join. When those hold the two runs are one word and no
    /// space must be synthesized. This works purely on the assembled text — the
    /// spans are left unmerged, so page layout and table detection are
    /// unaffected (a span merge here would shift XY-cut/table statistics).
    fn is_reliable_kerning_overlap(prev: &TextSpan, span: &TextSpan, gap: f32) -> bool {
        let fs = prev.font_size.max(span.font_size).max(1.0);
        let prev_last = prev.text.chars().next_back();
        let next_first = span.text.chars().next();
        gap < 0.0
            && gap > -fs
            && prev.font_name == span.font_name
            && prev.font_weight == span.font_weight
            && prev.is_italic == span.is_italic
            && prev_last.is_some_and(|c| c.is_alphanumeric())
            && next_first.is_some_and(|c| c.is_alphanumeric())
            // A lowercase→uppercase transition at the join is a word/sentence
            // boundary ("...with"+"Gp53", "Alg"+"The"), never the middle of a
            // single word split by kerning — real intra-word splits continue in
            // the same case tier ("PLANAL"+"TINA", "eigenv"+"alue"). Excluding
            // it keeps the two overlapping runs as separate words with a space.
            && !(prev_last.is_some_and(|c| c.is_lowercase())
                && next_first.is_some_and(|c| c.is_uppercase()))
            && {
                // Real proportional font metrics take many distinct per-glyph
                // advances; a missing-/Widths fallback emits ONE uniform
                // advance, and coarse/artifact width tables only a couple.
                // Require at least THREE distinct advances so a genuine
                // proportional run ("PLANAL": 6.67/5.56/7.22) is accepted while
                // a 1- or 2-value fallback table (which manufactures fake
                // overlaps between separate words) is not.
                let mut distinct: [i32; 3] = [i32::MIN, i32::MIN, i32::MIN];
                let mut n = 0usize;
                for w in prev.char_widths.iter().map(|w| (w * 100.0).round() as i32) {
                    if !distinct[..n].contains(&w) {
                        if n < 3 {
                            distinct[n] = w;
                        }
                        n += 1;
                        if n >= 3 {
                            break;
                        }
                    }
                }
                n >= 3
            }
    }

    /// # Returns
    /// `true` if a space should be inserted between the spans
    fn should_insert_space(prev: &TextSpan, current: &TextSpan) -> bool {
        // Get font size (use the larger of the two)
        let font_size = prev.font_size.max(current.font_size).max(1.0);

        // Same-line gate. Uses the shared threshold so the assembly
        // loop's same-line decision and the space-insertion decision
        // cannot disagree about where a line ends.
        let y_diff = (prev.bbox.y - current.bbox.y).abs();
        if y_diff > Self::same_line_threshold(prev, current) {
            return false; // Different lines - no space needed
        }

        // CJK scripts (Chinese, Japanese, Korean) do not use spaces between
        // words. If both the tail of prev and the head of current are CJK characters,
        // inserting a space would produce incorrect tokenisation.
        let prev_tail = prev.text.chars().next_back();
        let curr_head = current.text.chars().next();
        let is_cjk = |c: char| {
            matches!(
                c as u32,
                0x3040..=0x309F   // Hiragana
                | 0x30A0..=0x30FF // Katakana
                | 0x3400..=0x4DBF // CJK Unified Ideographs Extension A
                | 0x4E00..=0x9FFF // CJK Unified Ideographs
                | 0xAC00..=0xD7AF // Hangul Syllables
                | 0x20000..=0x2A6DF // CJK Unified Ideographs Extension B
                | 0xFF00..=0xFFEF // Halfwidth and Fullwidth Forms
                | 0x3000..=0x303F // CJK Symbols and Punctuation
            )
        };
        if prev_tail.is_some_and(is_cjk) && curr_head.is_some_and(is_cjk) {
            return false;
        }

        // Complex Brahmic / South-East-Asian scripts (Devanagari, Bengali,
        // Tamil, Telugu, …, Thai, Khmer): an inter-glyph gap *inside* a word is
        // not a word break. These scripts render dependent vowel signs
        // (matras), conjuncts, and reordered glyphs with their own positional
        // advances, so the Latin-tuned proportional-gap test below fires inside
        // a syllable cluster (e.g. a Bengali consonant following a wide matra
        // sits ~0.7em from it). Word boundaries in conforming text are carried
        // by an explicit SPACE glyph — ISO 32000-1 §14.8.2.5 requires the
        // spacing characters that separate words to be present — so a heuristic
        // space here only double-counts a boundary the explicit space already
        // marks. Suppress it when both sides are the *same* complex script;
        // this mirrors the CJK guard above (CJK uses no inter-word space at
        // all, these scripts carry it explicitly).
        {
            use crate::text::complex_script_detector::detect_complex_script;
            let prev_script = prev_tail.and_then(|c| detect_complex_script(c as u32));
            let curr_script = curr_head.and_then(|c| detect_complex_script(c as u32));
            if let (Some(p), Some(c)) = (prev_script, curr_script) {
                if p == c {
                    return false;
                }
            }
        }

        // Emoji / pictographic → letter boundary: a wide pictographic glyph
        // (e.g. 📄) abuts the next token, so the proportional-gap test below
        // would drop the inter-token space (`📄README` instead of `📄 README`).
        // Word boundaries are reader latitude (ISO 32000-1:2008 §9.10); keep the
        // space. The alphabetic-follower requirement excludes combined ZWJ/VS
        // emoji sequences (whose next char is a selector or another pictograph).
        if prev_tail.is_some_and(crate::extractors::text::is_pictographic)
            && curr_head.is_some_and(char::is_alphabetic)
        {
            return true;
        }

        // Calculate horizontal gap
        let prev_end_x = prev.bbox.x + prev.bbox.width;
        let gap = current.bbox.x - prev_end_x;

        // CJK script ↔ non-CJK boundary: pdftotext (and the GT it produces)
        // inserts a space wherever a CJK *script* glyph (ideograph, kana, or
        // hangul) meets a Latin/digit character on the same line, regardless
        // of how tightly the two were typeset. Without this, mixed-script
        // content like "神鹰集团" + "2015" collapses into one token
        // "神鹰集团2015", which never matches GT's separate "神鹰集团"
        // "2015" tokens (issue 484, pr-136).
        //
        // IMPORTANT: this MUST exclude fullwidth ASCII variants (U+FF01..FF5E
        // — ＜＞＝＠ etc.) and CJK Symbols and Punctuation (U+3000..303F) even
        // though they are technically "CJK characters". Those are *operator*
        // glyphs that sit inline with adjacent digits and Latin in CJK
        // technical documents — pdftotext keeps "60000≤Q＜80000"
        // "20＜μ≤30" as compound tokens (issue 484, issue-336). Forcing a
        // boundary space there destroys the compound and regresses Jaccard.
        let is_cjk_script = |c: char| {
            matches!(
                c as u32,
                0x3040..=0x309F      // Hiragana
                | 0x30A0..=0x30FF    // Katakana
                | 0x3400..=0x4DBF    // CJK Unified Ideographs Extension A
                | 0x4E00..=0x9FFF    // CJK Unified Ideographs
                | 0xAC00..=0xD7AF    // Hangul Syllables
                | 0x20000..=0x2A6DF  // CJK Unified Ideographs Extension B
                | 0xFF66..=0xFF9F    // Halfwidth Katakana
            )
        };
        let crosses_cjk_boundary = match (prev_tail, curr_head) {
            (Some(p), Some(c)) => is_cjk_script(p) != is_cjk_script(c),
            _ => false,
        };
        // ASCII punctuation hugs the preceding token in every script —
        // pdftotext's GT renders "する." with no space and "神鹰，2015"
        // with no space before the comma either. Suppress the boundary
        // forced-space when the transitioning glyph IS the punctuation;
        // the space-threshold path below still handles real gaps.
        let is_clause_punct = |c: char| {
            matches!(
                c,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}'
                // Indic danda / double danda (sentence terminators that hug the
                // preceding token like a Latin full stop). The danda lives in the
                // Devanagari block but is shared by Bengali, Gurmukhi, etc., so a
                // Bengali sentence + danda would otherwise read as a script
                // transition and take a geometric space ("প্রাণী ।").
                | '\u{0964}' | '\u{0965}'
                // Arabic comma / semicolon / question mark (RTL clause punctuation).
                | '\u{060C}' | '\u{061B}' | '\u{061F}'
            )
        };
        let punct_at_boundary = curr_head.is_some_and(is_clause_punct)
            || prev_tail.is_some_and(|c| matches!(c, '(' | '[' | '{'));
        // Hangul↔digit is NOT a word boundary: a Korean numeral hugs its
        // Sino-Korean counter ("1만년" = 10,000 years, "약 1만년"). Unlike a
        // Chinese ideograph meeting a Latin year ("神鹰集团" + "2015", which
        // pdftotext splits, issue 484), Korean keeps the digit and counter as
        // one token, so forcing a space here over-segments the eojeol.
        let is_hangul = |c: char| (0xAC00..=0xD7AF).contains(&(c as u32));
        let hangul_digit_boundary = match (prev_tail, curr_head) {
            (Some(p), Some(c)) => {
                (is_hangul(p) && c.is_ascii_digit()) || (p.is_ascii_digit() && is_hangul(c))
            }
            _ => false,
        };
        if crosses_cjk_boundary
            && !punct_at_boundary
            && !hangul_digit_boundary
            && gap > -0.5
            && gap < font_size * 5.0
        {
            return true;
        }

        // Space threshold: 0.15 × font size
        // Typical space width is ~0.25em, so 0.15em catches gaps > 60% of a space.
        // This aligns with the text extractor's font-aware threshold (~50% of space width).
        let space_threshold = font_size * 0.15;

        // Insert space if gap is significant. Previously the upper bound was
        // `gap < font_size * 5.0` on the rationale that very large gaps mean
        // "column boundary, no space needed" — but downstream the caller
        // concatenates the two spans together when this returns false, so
        // "column boundary" actually rendered as `3.80%4.41%` on wide rate
        // tables (issue 487 pr-138-example.pdf). Drop the upper bound so any
        // gap above the inter-glyph threshold gets at least a single space.
        //
        // Clause punctuation hugs the preceding word in Brahmic scripts. The
        // producer leaves a wide advance after a Bengali/Devanagari syllable
        // (matra/akhand positioning), so the geometric test would float a danda
        // ("প্রাণী ।") or a comma ("रोशनी ,") off as its own token. Scope this to
        // a *complex-script* previous glyph (or an Indic danda) so the universal
        // Latin/math/form paths — where the same suppression interacts badly with
        // the forward-gap line-break heuristic — stay byte-for-byte unchanged.
        {
            use crate::text::complex_script_detector::detect_complex_script;
            let prev_is_complex = prev_tail
                .and_then(|c| detect_complex_script(c as u32))
                .is_some();
            let curr_is_indic_punct =
                curr_head.is_some_and(|c| matches!(c, '\u{0964}' | '\u{0965}'));
            if curr_head.is_some_and(is_clause_punct) && (prev_is_complex || curr_is_indic_punct) {
                return false;
            }
        }
        gap > space_threshold
    }

    /// Stacked two-line column/table-header cell detector, applied ONLY on the
    /// structure-tree (tagged-content) assembly path — never the main flow.
    ///
    /// A tagged table can draw a header cell as two stacked rows ("Comparison"
    /// over "rate"). When the structure-tree assembler linearizes the cell's
    /// spans it sees them as consecutive, horizontally OVERLAPPING (negative
    /// gap) spans whose baseline drop stayed just under `same_line_threshold`,
    /// so it treats them as one line and — because the gap is negative —
    /// `should_insert_space` returns false and they fuse ("Comparisonrate").
    /// A negative gap combined with a genuine baseline shift is two stacked
    /// tokens, never intra-word kerning (which shares a baseline), so a space
    /// is warranted.
    ///
    /// This deliberately lives OUTSIDE `should_insert_space`: the main flow
    /// (untagged PDFs, e.g. LaTeX math) already routes backtracking
    /// baseline-shifted runs — a fraction's numerator over its denominator —
    /// through dedicated newline branches before the space decision, and adding
    /// this rule there fragments equations. Scoping it to the tagged path keeps
    /// those inputs byte-identical while fixing stacked header cells.
    fn stacked_cell_needs_space(prev: &TextSpan, current: &TextSpan) -> bool {
        let font_size = prev.font_size.max(current.font_size).max(1.0);
        let y_diff = (prev.bbox.y - current.bbox.y).abs();
        let gap = current.bbox.x - (prev.bbox.x + prev.bbox.width);
        // Under the caller's same-line band (else the caller line-breaks), a
        // real baseline shift (> 0.5 em) with horizontal overlap (negative gap)
        // is a stacked cell. Both sides must be alphanumeric word content, not
        // punctuation/symbol runs.
        gap < -0.5
            && y_diff > font_size * 0.5
            && prev
                .text
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric())
            && current
                .text
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric())
    }

    /// Detect a span whose text is `N.M` (all-digit groups around one dot) and whose
    /// bbox.width is >40% larger than char_widths imply. This pattern occurs in
    /// sailing-score / competition-table PDFs where two adjacent columns (e.g. Q8=1,
    /// F9=10) are stored as a single Tj text run "1.10" spanning both column cells.
    /// Reference ground truth tokenises them as separate words; we must split at the dot.
    pub(crate) fn is_column_spanning_decimal(span: &TextSpan) -> bool {
        let text = &span.text;
        let dot_pos = match text.find('.') {
            Some(p) if p > 0 && p < text.len() - 1 => p,
            _ => return false,
        };
        if text[dot_pos + 1..].contains('.') {
            return false;
        }
        if !text[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        if !text[dot_pos + 1..].chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        let char_count = text.chars().count();
        // Signal 1: sparse char_widths array. When the font's glyph
        // iteration produces fewer advance-width entries than there are
        // characters in the decoded string, the span was assembled from two
        // (or more) concatenated Tj runs whose widths come from different
        // points in the glyph table. This is the exact pattern issue 487
        // nougat_018 sailing-score grids hit: each score cell is emitted as
        // a single Tj like `1.10` with `char_widths=[w]` while the PDF
        // semantically means "1" followed by "10" in adjacent score
        // columns. bbox.width can still be tight here (the producer set
        // it to cover just the rendered glyph run), so the existing
        // bbox-inflation check below misses these. Catch them via the
        // sparse-cw signal directly.
        if !span.char_widths.is_empty() && span.char_widths.len() < char_count {
            return true;
        }
        let expected_width = if !span.char_widths.is_empty() {
            let cw_sum: f32 = span.char_widths.iter().sum();
            cw_sum * (char_count as f32 / span.char_widths.len() as f32)
        } else if span.font_size > 0.0 {
            // Digits are narrower than average; 0.50em per char is a safe
            // upper bound for all-digit strings (avoids the 0.60 fallback
            // producing false negatives on column-spanning sailing scores
            // when char_widths is empty, e.g. word_spans from extract_words).
            span.font_size * 0.50 * char_count as f32
        } else {
            return false;
        };
        // Use absolute gap (bbox_w - expected) rather than a ratio so that
        // 5-char spans like "12.11" (gap ≈ 1.1×fs) are caught along with
        // 4-char spans like "1.10" (gap ≈ 1.4×fs). 1.0×font_size is a safe
        // lower bound: normal text rarely has >1em of hidden whitespace.
        let gap = span.bbox.width - expected_width;
        span.font_size > 0.0 && gap > span.font_size * 1.0
    }

    /// When a CID font's glyph iteration produces fewer advance-width entries than
    /// `decode_text_to_unicode` produces unicode chars, `char_widths.len()` < char count.
    /// This indicates two concatenated text runs stored in one Tj operator (e.g. "Theorem1.7"
    /// where "Theorem" widths come from the font's glyph table and "1.7" doesn't have
    /// matching glyph entries). Return the byte offset at which to insert a space,
    /// or None if no split is appropriate.
    pub(crate) fn char_widths_boundary_split(span: &TextSpan) -> Option<usize> {
        let cw_len = span.char_widths.len();
        if cw_len == 0 {
            return None;
        }
        let char_count = span.text.chars().count();
        if cw_len >= char_count {
            return None;
        }
        // Find the byte offset of the (cw_len)-th character
        let (boundary_byte, boundary_char) = span.text.char_indices().nth(cw_len)?;
        let prev_char = span.text[..boundary_byte].chars().next_back()?;
        // Don't insert if either side is already a space
        if boundary_char == ' ' || prev_char == ' ' {
            return None;
        }
        // Non-ASCII chars at the boundary are encoding artifacts (e.g. Polish diacritics
        // in Latin-2 / CP1250 fonts producing one fewer char_width entry). Only split
        // when the boundary char is ASCII, indicating a genuine text-run concatenation.
        if !boundary_char.is_ascii() {
            return None;
        }
        // Split at letter→digit boundary (e.g. "Theorem1.7") or lower→upper ASCII
        // case boundary (e.g. "BigText" from concatenated CID runs "Big"+"Text").
        // Upper→lower transitions are excluded: a ligature spanning an upper→lower
        // boundary within a compound word (e.g. "officeMax" with "fl" ligature)
        // would otherwise produce a false split.
        if (prev_char.is_alphabetic() && boundary_char.is_ascii_digit())
            || (prev_char.is_ascii_lowercase() && boundary_char.is_ascii_uppercase())
        {
            Some(boundary_byte)
        } else {
            None
        }
    }

    /// Merge subscript and superscript spans into their base span.
    ///
    /// In math-heavy untagged PDFs, subscript glyphs (e.g. the "1" in "k₁") are
    /// stored as separate `TextSpan` entries at a slightly lower/higher baseline than
    /// the base character, and non-adjacent in reading order. The text assembly loop
    /// emits them as isolated tokens ("k … 1") rather than the expected word ("k1").
    ///
    /// A span is classified as a subscript/superscript when ALL of the following hold:
    ///  - 1–3 ASCII alphanumeric chars (digit or letter, no punctuation)
    ///  - font_size < 85 % of the page's maximum font size
    ///  - There exists a preceding "base" span whose right edge (x + width) is within
    ///    ±0.6 × sub_fs of the subscript's left edge (x-adjacent)
    ///  - The vertical offset between base and sub is in [8 %, 85 %] of base_fs
    ///    (distinguishes true sub/superscripts from same-line small caps)
    ///
    /// Matched subscript/superscript spans have their text appended to the base
    /// are removed from `spans`.
    fn merge_sub_superscript_spans(spans: &mut Vec<TextSpan>) {
        let n = spans.len();
        if n < 2 {
            return;
        }
        let max_fs = spans.iter().map(|s| s.font_size).fold(0f32, f32::max);
        if max_fs <= 0.0 {
            return;
        }

        // Item 5b (M4): an INDEX CLUSTER is a comma-joined run of digits that the
        // producer set as a single subscript/superscript — an F-statistic's
        // degrees of freedom (`4,176` in `F4,176`) or a multi-affiliation marker
        // (`1,2`). These exceed the 3-char limit and contain a comma, so the plain
        // sub-char gate rejected them, stranding `F`, `4`, `176` as separate
        // tokens. Recognised here so the comma cluster merges back into its base.
        let is_index_cluster = |t: &str| -> bool {
            t.chars().count() >= 3
                && t.contains(',')
                && t.chars().all(|c| c.is_ascii_digit() || c == ',')
                && !t.starts_with(',')
                && !t.ends_with(',')
        };

        // For each candidate sub/superscript span, record which base span to merge into.
        let mut to_merge: Vec<(usize, usize)> = Vec::new(); // (base_idx, sub_idx)
        let mut already_sub: std::collections::HashSet<usize> = std::collections::HashSet::new();

        for i in 0..n {
            let sub = &spans[i];
            // Char-count gate (not byte-count): U+00B2/B3/B9 are 2-byte
            // UTF-8 sequences and U+2070..U+209F are 3-byte, so the
            // earlier byte-length check would have dropped a legitimate
            // 3-digit Unicode subscript like "₁₂₃" (9 bytes).
            if sub.text.is_empty() || (sub.text.chars().count() > 3 && !is_index_cluster(&sub.text))
            {
                continue;
            }
            // Accept the raw ASCII the extractor produces AND the
            // already-substituted Unicode super/subscript codepoints
            // (apply_super_sub_script_substitutions runs upstream).
            // Without the U+00B2/B3/B9 + U+2070..U+209F gate, a
            // chemistry formula like "H₂O" would lose the subscript
            // span from this merge, leaving "H ₂ O" in the output.
            let is_sub_char = |c: char| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '\u{00B2}' | '\u{00B3}' | '\u{00B9}')
                    || ('\u{2070}'..='\u{209F}').contains(&c)
            };
            // M4 (item 5c): a span the producer explicitly raised/lowered with the
            // Text Rise operator (ISO 32000-1 §9.3.7 `Ts`) is an authoritative
            // sub/superscript even when it is NOT shrunk and is not in the ASCII /
            // Unicode sub-glyph set (e.g. a math operator superscript). `text_rise`
            // is stored as the Ts/font-size ratio, so |ratio| ≥ 0.10 marks a real
            // shift. Such a span bypasses the charset and font-size gates below; the
            // x/y proximity gates in the base search still apply, so a genuinely
            // detached different-row marker is not over-merged.
            let ts_flagged = sub.text_rise.abs() >= 0.10;
            if !ts_flagged && !sub.text.chars().all(is_sub_char) && !is_index_cluster(&sub.text) {
                continue;
            }
            // Must be clearly smaller than the dominant font on this page (unless
            // the producer flagged it via Ts).
            if !ts_flagged && sub.font_size >= max_fs * 0.80 {
                continue;
            }
            let sub_fs = sub.font_size;
            let sub_x = sub.bbox.x;
            let sub_y = sub.bbox.y;

            // A purely NUMERIC sub-run (digits, optionally comma-joined) at a
            // base's advance edge is an inline super/subscript even when its
            // bbox shares the base's baseline. Some producers raise a glyph with
            // a small font but emit it on the SAME text-line baseline (the
            // visual rise lives in the glyph's own bbox, not the line's), so the
            // extractor records y_diff_abs ≈ 0. The 12 %-of-em vertical lower
            // bound (which screens out same-line small caps) would then strand
            // the marker — e.g. the isotope label `123` in `[123I]FP-CIT`, or an
            // author-affiliation marker `1,2`. Small caps are never bare digits,
            // so dropping the lower bound for numeric subs is safe; the smaller-
            // font, x-edge, valid-base, and upper-y gates still apply.
            let sub_is_numeric =
                sub.text.chars().all(|c| c.is_ascii_digit() || c == ',') && !ts_flagged;

            // Search backwards for the best-matching base span.
            let search_limit = 30.min(i);
            let mut best: Option<(usize, f32)> = None; // (idx, |x_dist|)

            for j in (i.saturating_sub(search_limit)..i).rev() {
                if already_sub.contains(&j) {
                    continue;
                }
                let base = &spans[j];
                // Base must be at least 25 % larger than the sub (sub_fs ≤ 0.80×base_fs),
                // UNLESS the producer flagged the sub via Ts (then it may be the same
                // size as its base — the rise itself, not the size, marks it).
                if !ts_flagged && base.font_size < sub_fs * 1.25 {
                    continue;
                }
                // Base span must be a valid subscript host:
                //   • 1-char bases (single math variable: k, γ, ρ, H, ∆, …)
                //   • 2-char bases that are NOT two lowercase-ASCII letters
                //     (accepts "Pr", "εp", "ρε" but rejects "of", "to")
                //   • longer bases ENDING in an acronym — a run of ≥2 trailing
                //     uppercase ASCII letters (e.g. a wide body span
                //     "…activation of VPAC", or "CA1"'s "CA"). Receptor/region
                //     names (VPAC, CA, PAC, GABA, NMDA, …) carry a subscript on
                //     their trailing acronym, but the producer emits the whole
                //     wrapped line as one span, so the ≤2-char gate stranded the
                //     subscript and the row-band sort glued it onto a later word
                //     ("…of VPAC receptors … pyramidal1"). The subscript text is
                //     appended to the base's END, which is exactly the acronym, so
                //     it reconstructs "VPAC1". The x-edge gate below still requires
                //     the sub to sit at the base's advance edge, and the trailing
                //     run being UPPERCASE keeps ordinary prose (which ends in a
                //     lowercase letter or punctuation) from ever matching.
                // Multi-char lowercase-only strings like "and", "let", "sup"
                // are English words or common operators; their adjacent digit
                // spans are handled by the assembly loop and char_widths_boundary_split.
                let chars: Vec<char> = base.text.chars().collect();
                let ends_in_acronym = || {
                    let trailing_upper = chars
                        .iter()
                        .rev()
                        .take_while(|c| c.is_ascii_uppercase())
                        .count();
                    trailing_upper >= 2
                };
                let is_valid_base = match chars.len() {
                    1 => true,
                    2 => chars.iter().any(|c| !c.is_ascii_lowercase()),
                    _ => ends_in_acronym(),
                };
                if !is_valid_base {
                    continue;
                }
                let base_right = base.bbox.x + base.bbox.width;
                let x_dist = sub_x - base_right;
                let y_diff_abs = (base.bbox.y - sub_y).abs();

                // Use em-relative x_dist thresholds.
                // Real sub/superscript glyphs land within ±[−0.1×base_fs, 0.25×base_fs]
                // of the base's advance edge; absolute bounds were wrong for non-12pt fonts.
                let base_fs = base.font_size.max(1.0);
                let x_lo = -0.1 * base_fs;
                let x_hi = 0.25 * base_fs;
                if x_dist < x_lo || x_dist > x_hi {
                    continue;
                }
                // Vertical offset must be in the sub/superscript range.
                // Lower bound 12 % of base_fs ensures same-line small caps are excluded.
                // Upper bound 75 % excludes large line-to-line y differences (e.g.
                // author affiliation numbers on a different baseline row).
                // Numeric subs (digits/commas) may sit on the base baseline, so
                // skip the small-caps lower bound for them; all other subs keep it.
                let y_lo = if sub_is_numeric {
                    0.0
                } else {
                    base.font_size * 0.12
                };
                if y_diff_abs < y_lo || y_diff_abs > base.font_size * 0.75 {
                    continue;
                }
                let score = x_dist.abs();
                if best.is_none() || score < best.unwrap().1 {
                    best = Some((j, score));
                }
            }

            if let Some((base_idx, _)) = best {
                to_merge.push((base_idx, i));
                already_sub.insert(i);
            }
        }

        if to_merge.is_empty() {
            return;
        }

        // Collect (base_idx, sub_idx, sub_text, sub_right_edge, sub_char_widths, sub_fs)
        // before mutating spans.
        let ops: Vec<(usize, usize, String, f32, Vec<f32>, f32)> = to_merge
            .iter()
            .map(|pair| {
                let (bi, si) = *pair;
                let sub = &spans[si];
                (
                    bi,
                    si,
                    sub.text.clone(),
                    sub.bbox.x + sub.bbox.width,
                    sub.char_widths.clone(),
                    sub.font_size,
                )
            })
            .collect();

        // Apply: append sub text to base; extend bbox and char_widths to cover the sub.
        //
        // Extending bbox: the assembly loop uses span widths for gap calculations — keeping
        // the original width would make the gap to the following span appear too large.
        //
        // Extending char_widths: char_widths_boundary_split fires whenever cw_len < char_count.
        // After merging sub text, char_count grows but cw_len stays the same, which would
        // cause the split to re-separate the merged token (e.g. "k1" → "k 1"). Adding
        // estimated widths for the sub characters prevents this.
        for (base_idx, _, sub_text, sub_right, sub_cw, sub_fs) in &ops {
            let base = &mut spans[*base_idx];
            base.text.push_str(sub_text);
            let base_right = base.bbox.x + base.bbox.width;
            if *sub_right > base_right {
                base.bbox.width = sub_right - base.bbox.x;
            }
            if !base.char_widths.is_empty() {
                let sub_char_count = sub_text.chars().count();
                if !sub_cw.is_empty() {
                    base.char_widths.extend_from_slice(sub_cw);
                } else {
                    // Estimate sub char widths at 0.50 em per character.
                    let w = sub_fs * 0.50;
                    for _ in 0..sub_char_count {
                        base.char_widths.push(w);
                    }
                }
            }
        }

        // Drop the merged sub/superscript spans in one pass.
        let to_remove: std::collections::HashSet<usize> =
            ops.iter().map(|(_, si, _, _, _, _)| *si).collect();
        let mut idx = 0usize;
        spans.retain(|_| {
            let keep = !to_remove.contains(&idx);
            idx += 1;
            keep
        });
    }

    /// Append span text to `out`, splitting merged runs for cleaner word tokenisation.
    /// Priority 0: spans whose text is entirely `\n`/`\r` are line-break signals.
    /// Priority 1: column-spanning decimal (nougat_018 sailing tables).
    /// Priority 2: char_widths boundary split (pdfa_004 CID-font merge artifacts).
    #[inline]
    pub(crate) fn push_span_text(out: &mut String, span: &TextSpan) {
        // A span whose entire text is one or more newline/CR characters is a
        // ToUnicode line-break signal. Treat it as a logical newline separator rather
        // than emitting the raw control characters verbatim as visible content.
        if !span.text.is_empty() && span.text.chars().all(|c| c == '\n' || c == '\r') {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            return;
        }
        if Self::is_column_spanning_decimal(span) {
            let dot = span.text.find('.').unwrap();
            out.push_str(&span.text[..dot]);
            out.push(' ');
            out.push_str(&span.text[dot + 1..]);
        } else if let Some(split) = Self::char_widths_boundary_split(span) {
            out.push_str(&span.text[..split]);
            out.push(' ');
            out.push_str(&span.text[split..]);
        } else {
            out.push_str(&span.text);
        }
    }

    /// #557a: append a span's text to the structure-tree assembly, reversing a
    /// PURE-RTL run (every non-space char is an Arabic/Hebrew letter, no Latin)
    /// from visual to logical order. The tagged/struct-tree path collapses each
    /// run to a single span and never reaches `reverse_rtl_visual_order_runs`,
    /// so visually-stored RTL (e.g. issue10301 Hebrew "גבא") otherwise leaked
    /// out reversed. A single-direction run's logical order is just its reverse,
    /// so no glyph geometry is needed for the pure-RTL case.
    fn push_span_text_bidi(out: &mut String, span: &TextSpan, rtl_run: bool) {
        use crate::text::rtl_detector::is_rtl_text;
        // A span whose glyphs were drawn right-to-left (logical storage — the
        // producer positioned each glyph individually at decreasing x, ISO
        // 32000-1 §14.8.2.3.3 method 1; detected by `detect_rtl_draw_direction`)
        // already carries its CHARACTERS in LOGICAL order. The visual→logical
        // character reversal below assumes VISUAL storage and would corrupt
        // them, so emit the text verbatim — the letters are already correct.
        // (Word ORDER within such a span is left as the reading-order sort
        // produced it: a producer may emit words logically or visually within
        // the same document, so a blanket word reversal would corrupt the
        // logical ones.) Visual storage — the default — is never flagged and
        // keeps the character-reversal below, so it stays byte-identical.
        if span.rtl_draw_logical {
            Self::push_span_text(out, span);
            return;
        }
        let mut rtl = 0usize;
        let mut has_latin = false;
        for c in span.text.chars() {
            if c.is_whitespace() {
                continue;
            }
            if c.is_ascii_alphabetic() {
                has_latin = true;
                break;
            }
            if is_rtl_text(c as u32) {
                rtl += 1;
            }
        }
        if rtl >= 2 && !has_latin {
            let mut tmp = span.clone();
            // Strip producer-inserted SPACEs that fall *between two Arabic
            // letters* inside a single show string. ISO 32000-1 §14.8.2.3.3
            // states a reverse-order show string "shall not contain interior
            // SPACEs" — a word break is signalled by a SPACE at the string
            // boundary (here, a separate span), never inside it. Arabic is
            // cursive, so an interior space splits letters that the script
            // joins; it is never a word boundary in the pure-text
            // representation (§14.8.2.5). Restricted to Arabic (cursive): a
            // non-cursive script such as Hebrew can legitimately carry a
            // space-separated pair in one show string, so it is left alone.
            tmp.text =
                Self::reverse_rtl_keeping_marks(&Self::strip_interior_arabic_spaces(&span.text))
                    .replace(Self::RTL_WORD_BOUNDARY, " ");
            Self::push_span_text(out, &tmp);
        } else if rtl_run && Self::is_reversible_rtl_neutral_span(&span.text) {
            // A neutral-only span (separator / terminator punctuation plus
            // spaces — no strong letters and no digits) embedded in a pure-RTL
            // run carries its glyphs in *visual* (content-stream draw) order.
            // Per UAX #9 the neutrals inherit the surrounding right-to-left
            // direction (rules N1/N2), so their logical order is the reverse of
            // the visual sequence: a visual "<space><comma>" drawn between two
            // Hebrew words becomes "<comma><space>", re-attaching the comma to
            // the preceding word. The pure-RTL words around it are reversed by
            // the branch above; without this the punctuation stayed stranded on
            // the wrong side of the inter-word space.
            let mut tmp = span.clone();
            tmp.text = span.text.chars().rev().collect();
            Self::push_span_text(out, &tmp);
        } else if rtl_run && Self::is_reversible_rtl_numeric_span(&span.text) {
            // A neutral+numeric span (e.g. a Hebrew-context " ,2009-" or " 600-")
            // embedded in a pure-RTL run carries its glyphs in *visual*
            // (content-stream draw) order. Reverse it to logical order while
            // keeping each digit run forward (UAX #9 rule L2): visual " ,2009-"
            // → logical "-2009, ", re-attaching the hyphen to the number and the
            // comma to the preceding word, without ever flipping 2009 → 9002.
            let mut tmp = span.clone();
            tmp.text = crate::text::bidi::reverse_rtl_keep_numbers(&span.text);
            Self::push_span_text(out, &tmp);
        } else {
            Self::push_span_text(out, span);
        }
    }

    /// Whether `text` is a neutral+numeric span eligible for number-preserving
    /// RTL visual→logical reversal in [`push_span_text_bidi`]: every non-space
    /// char is a [reorderable neutral](Self::is_rtl_reorderable_neutral), an
    /// ASCII hyphen-minus, or a digit (ASCII / Arabic-Indic U+0660–0669 /
    /// Extended Arabic-Indic U+06F0–06F9); it contains **exactly one** maximal
    /// digit run (so a date range `2009-2010` or an ORCID is never reversed),
    /// at least one movable neutral/hyphen, and the number-preserving reversal
    /// actually changes it (else the cheaper verbatim path is byte-identical).
    fn is_reversible_rtl_numeric_span(text: &str) -> bool {
        let is_digit = |c: char| {
            c.is_ascii_digit()
                || ('\u{0660}'..='\u{0669}').contains(&c)
                || ('\u{06F0}'..='\u{06F9}').contains(&c)
        };
        let mut has_movable = false;
        let mut digit_runs = 0usize;
        let mut in_digit = false;
        for c in text.chars() {
            if is_digit(c) {
                if !in_digit {
                    digit_runs += 1;
                    in_digit = true;
                }
                continue;
            }
            in_digit = false;
            if c.is_whitespace() {
                continue;
            }
            if c == '-' || Self::is_rtl_reorderable_neutral(c) {
                has_movable = true;
                continue;
            }
            return false; // strong letter, bracket, quote, etc. → not eligible
        }
        digit_runs == 1 && has_movable && crate::text::bidi::reverse_rtl_keep_numbers(text) != text
    }

    /// Remove ASCII SPACE (U+0020) characters that sit *between two Arabic
    /// letters* within a single show string — producer-inserted spurious
    /// spaces that split a cursive word (e.g. `قِ ل` inside `القِطّ`).
    ///
    /// Per ISO 32000-1 §14.8.2.3.3 a show string "shall not contain interior
    /// SPACEs"; a genuine word break is a SPACE at a string boundary (a
    /// separate span in this pipeline). Combining marks between the space and
    /// its neighbouring base letter are seen through, so a mark sitting next to
    /// the space does not hide the Arabic letter on that side. Leading and
    /// trailing spaces (real word-break candidates) and spaces flanked by
    /// anything other than two Arabic letters are preserved verbatim, so the
    /// fast path returns the input unchanged when there is nothing to strip.
    fn strip_interior_arabic_spaces(text: &str) -> String {
        use crate::text::rtl_detector::{is_arabic_letter, is_rtl_diacritic};
        if !text.contains(' ') {
            return text.to_string();
        }
        // First non-mark char in `it` is an Arabic letter? (marks are seen
        // through so a diacritic next to the space does not hide its base.)
        fn arabic_letter_past_marks<'a>(it: impl Iterator<Item = &'a char>) -> bool {
            for &c in it {
                if is_rtl_diacritic(c as u32) {
                    continue;
                }
                return is_arabic_letter(c as u32);
            }
            false
        }
        let chars: Vec<char> = text.chars().collect();
        // Interior spaces flanked by Arabic letters on both sides.
        let qualifying: Vec<usize> = (0..chars.len())
            .filter(|&i| {
                chars[i] == ' '
                    && arabic_letter_past_marks(chars[..i].iter().rev())
                    && arabic_letter_past_marks(chars[i + 1..].iter())
            })
            .collect();
        if qualifying.is_empty() {
            return text.to_string();
        }
        // SHATTER case (§14.8.2.3.3: a show-string must not contain interior
        // SPACEs). When a space sits between a MAJORITY of adjacent Arabic-letter
        // pairs, the producer exploded one cursive word into separate glyphs
        // (e.g. `فصيلة` drawn as `ة لي ص ف`); every interior space is spurious, so
        // strip them all. The density test (qualifying ≥ half the inter-letter
        // gaps) tells this apart from ordinary multi-word text, whose spaces are
        // sparse real word breaks (the right_to_left_01 class) — those stay.
        let arabic_letters = chars
            .iter()
            .filter(|&&c| is_arabic_letter(c as u32))
            .count();
        let gaps = arabic_letters.saturating_sub(1).max(1);
        if qualifying.len() >= 2 && qualifying.len() * 2 >= gaps {
            let drop: std::collections::HashSet<usize> = qualifying.iter().copied().collect();
            return chars
                .iter()
                .enumerate()
                .filter_map(|(i, &c)| (!drop.contains(&i)).then_some(c))
                .collect();
        }
        // Sparse case: a lone spurious cursive-join space. A span with several
        // sparse Arabic-flanked spaces is ordinary multi-word text whose spaces
        // are real word breaks — leave them intact.
        if qualifying.len() != 1 {
            return text.to_string();
        }
        let drop = qualifying[0];
        // Joining-type discriminator (§14.8.2.3.3). The cursive join already
        // breaks AFTER a right-joining-only letter (ا د ذ ر ز و …), so a space
        // there renders identically whether it is a genuine word break or a
        // producer artefact — the two are indistinguishable. Stripping it would
        // risk concatenating two real words (`دار اب` → `داراب`). Only a space
        // after a dual-joining letter unambiguously broke a join that should
        // not break, so restrict the strip to that case and keep the space when
        // the preceding base letter (seen past any combining marks) is
        // right-joining.
        let preceding_right_joining = chars[..drop]
            .iter()
            .rev()
            .find(|&&c| !is_rtl_diacritic(c as u32))
            .is_some_and(|&c| crate::text::rtl_detector::is_right_joining_arabic(c as u32));
        if preceding_right_joining {
            return text.to_string();
        }
        chars
            .iter()
            .enumerate()
            .filter_map(|(i, &c)| (i != drop).then_some(c))
            .collect()
    }

    /// Emit the inter-line newline(s) between two vertically separated spans in
    /// the struct-order assembler. A normal line gap maps to one to three
    /// newlines proportional to the vertical distance (`y_diff / line_height`,
    /// clamped) so multi-line paragraph spacing survives. When `single_break`
    /// is set — two consecutive cells of a tagged table on different rows — a
    /// single newline is emitted instead: table rows are stacked block rows,
    /// not free-leading paragraphs (ISO 32000-1 §14.8.4.3.4), and the geometric
    /// row pitch (~1.7× leading) would otherwise insert a spurious blank line
    /// between every row.
    fn push_line_breaks(
        text: &mut String,
        prev: &TextSpan,
        span: &TextSpan,
        y_diff: f32,
        single_break: bool,
    ) {
        if single_break {
            text.push('\n');
            return;
        }
        let font_size = prev.font_size.max(span.font_size).max(10.0);
        let line_height = font_size * 1.2;
        let num_breaks = (y_diff / line_height).round() as usize;
        for _ in 0..num_breaks.clamp(1, 3) {
            text.push('\n');
        }
    }

    /// Whether every span in this marked-content element is part of a *pure*
    /// right-to-left run: at least one Arabic/Hebrew letter is present and no
    /// Latin letter is. Mirrors the gating in [`order_mcid_spans`] (the branch
    /// that sorts pure-RTL spans right-to-left). Used to decide whether
    /// neutral-only punctuation spans inside the run must be reversed from
    /// visual to logical order by [`push_span_text_bidi`].
    fn mcid_run_is_pure_rtl(spans: &[crate::layout::TextSpan]) -> bool {
        use crate::text::rtl_detector::is_rtl_text;
        let has_rtl = spans
            .iter()
            .any(|s| s.text.chars().any(|c| is_rtl_text(c as u32)));
        let has_latin = spans
            .iter()
            .any(|s| s.text.chars().any(|c| c.is_ascii_alphabetic()));
        has_rtl && !has_latin
    }

    /// Is `c` a direction-neutral punctuation mark whose order inside an RTL
    /// run is a pure transposition — safe to reverse with the surrounding RTL
    /// neutrals? Restricted to separators and terminators (comma, full stop,
    /// semicolon, colon, exclamation, question, and their Arabic/Hebrew
    /// equivalents). Deliberately excludes paired brackets and quotation marks
    /// (which need UAX #9 L4 mirroring, handled elsewhere), digits, and any
    /// character that anchors an embedded left-to-right sub-run.
    fn is_rtl_reorderable_neutral(c: char) -> bool {
        matches!(
            c,
            ',' | '.' | ';' | ':' | '!' | '?'
                | '\u{05BE}' // Hebrew maqaf
                | '\u{05C3}' // Hebrew sof pasuq
                | '\u{060C}' // Arabic comma
                | '\u{061B}' // Arabic semicolon
                | '\u{061F}' // Arabic question mark
                | '\u{06D4}' // Arabic full stop
        )
    }

    /// Whether `text` is a neutral-only span eligible for the RTL visual→logical
    /// reversal in [`push_span_text_bidi`]: every character is whitespace or a
    /// [reorderable neutral](Self::is_rtl_reorderable_neutral), it contains at
    /// least one such punctuation mark, and it is at least two characters long
    /// (so there is an order to fix). A lone punctuation glyph or a bare space
    /// run reverses to itself and is left untouched.
    fn is_reversible_rtl_neutral_span(text: &str) -> bool {
        let mut has_punct = false;
        let mut count = 0usize;
        for c in text.chars() {
            count += 1;
            if c.is_whitespace() {
                continue;
            }
            if Self::is_rtl_reorderable_neutral(c) {
                has_punct = true;
                continue;
            }
            return false; // letter, digit, bracket, quote, or other → not eligible
        }
        has_punct && count >= 2
    }

    /// Reverse a pure-RTL run from visual to logical order while keeping each
    /// Arabic/Hebrew combining mark attached to its base letter.
    ///
    /// A naive `chars().rev()` reverses by Unicode scalar value, so a base
    /// letter's diacritics (which follow it in logical order — kasra/shadda
    /// U+0650/U+0651, Hebrew points U+05B0..) jump *in front* of the base and
    /// float off as standalone marks. Grouping each base char with the
    /// combining marks that trail it, then reversing the group order (each
    /// group's internal order preserved), keeps marks bound to their base.
    fn reverse_rtl_keeping_marks(text: &str) -> String {
        use crate::text::rtl_detector::is_rtl_diacritic;
        let mut groups: Vec<Vec<char>> = Vec::new();
        for c in text.chars() {
            if is_rtl_diacritic(c as u32) && !groups.is_empty() {
                groups.last_mut().unwrap().push(c);
            } else {
                groups.push(vec![c]);
            }
        }
        groups.iter().rev().flatten().collect()
    }

    /// Parse font size from a /DA (Default Appearance) string.
    ///
    /// DA strings follow the format: `"/FontName size Tf ..."` (e.g., `"/Helv 12 Tf 0 g"`).
    /// Returns the font size preceding the `Tf` operator, or a default of 10.0 if not found.
    fn parse_font_size_from_da(da: &str) -> f32 {
        let tokens: Vec<&str> = da.split_whitespace().collect();
        for i in 0..tokens.len() {
            if tokens[i] == "Tf" && i > 0 {
                if let Ok(size) = tokens[i - 1].parse::<f32>() {
                    if size > 0.0 {
                        return size;
                    }
                }
            }
        }
        10.0 // default
    }

    /// Extract widget annotation values as TextSpans positioned at their /Rect locations.
    ///
    /// Converts each widget annotation's field value into a `TextSpan` with the annotation's
    /// bounding box. These spans merge naturally with content stream spans and get positioned
    /// correctly by existing layout algorithms.
    fn extract_widget_spans(&self, page_index: usize) -> Vec<TextSpan> {
        use crate::extractors::forms::field_flags;
        use crate::geometry::Rect;

        let page_obj = match self.get_page(page_index) {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };
        let page_dict = match page_obj.as_dict() {
            Some(d) => d,
            None => return Vec::new(),
        };

        // Get /Annots array (may be direct or indirect)
        let annots_arr = match page_dict.get("Annots") {
            Some(Object::Array(arr)) => arr.clone(),
            Some(Object::Reference(r)) => match self.load_object(*r) {
                Ok(Object::Array(arr)) => arr,
                _ => return Vec::new(),
            },
            _ => return Vec::new(),
        };

        let mut spans = Vec::new();
        let base_sequence = 1_000_000; // high sequence number so widget spans sort after content spans at same Y

        for (idx, annot_obj) in annots_arr.iter().enumerate() {
            let annot_ref = match annot_obj {
                Object::Reference(r) => *r,
                _ => continue,
            };
            let dict = match self.load_object(annot_ref) {
                Ok(obj) => match obj.as_dict() {
                    Some(d) => d.clone(),
                    None => continue,
                },
                Err(_) => continue,
            };

            // Only process Widget annotations
            let subtype = match dict.get("Subtype").and_then(|s| s.as_name()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if !subtype.eq_ignore_ascii_case("widget") {
                continue;
            }

            // Check /F flags — skip invisible/hidden/noview annotations
            // Bit 1 (0x1) = Invisible, Bit 2 (0x2) = Hidden, Bit 6 (0x20) = NoView
            if let Some(Object::Integer(f)) = dict.get("F") {
                if *f & (0x1 | 0x2 | 0x20) != 0 {
                    continue;
                }
            }

            // Parse /Rect [x1, y1, x2, y2] → Rect { x, y, width, height }
            let rect = match dict.get("Rect") {
                Some(Object::Array(arr)) if arr.len() == 4 => {
                    let mut coords = [0.0f32; 4];
                    let mut ok = true;
                    for (i, item) in arr.iter().enumerate() {
                        match item {
                            Object::Integer(n) => coords[i] = *n as f32,
                            Object::Real(f) => coords[i] = *f as f32,
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if !ok {
                        continue;
                    }
                    let x = coords[0].min(coords[2]);
                    let y = coords[1].min(coords[3]);
                    let w = (coords[2] - coords[0]).abs();
                    let h = (coords[3] - coords[1]).abs();
                    if w < 0.1 || h < 0.1 {
                        continue;
                    } // skip zero-area rects
                    Rect::new(x, y, w, h)
                }
                Some(Object::Reference(r)) => match self.load_object(*r) {
                    Ok(Object::Array(arr)) if arr.len() == 4 => {
                        let mut coords = [0.0f32; 4];
                        let mut ok = true;
                        for (i, item) in arr.iter().enumerate() {
                            match item {
                                Object::Integer(n) => coords[i] = *n as f32,
                                Object::Real(f) => coords[i] = *f as f32,
                                _ => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if !ok {
                            continue;
                        }
                        let x = coords[0].min(coords[2]);
                        let y = coords[1].min(coords[3]);
                        let w = (coords[2] - coords[0]).abs();
                        let h = (coords[3] - coords[1]).abs();
                        if w < 0.1 || h < 0.1 {
                            continue;
                        }
                        Rect::new(x, y, w, h)
                    }
                    _ => continue,
                },
                _ => continue,
            };

            // Get field type via /FT (with parent-chain inheritance)
            let ft = dict
                .get("FT")
                .and_then(|o| o.as_name())
                .map(|s| s.to_string())
                .or_else(|| self.resolve_inherited_ft(&dict));

            // Get field flags /Ff (with parent-chain inheritance)
            let ff = dict
                .get("Ff")
                .and_then(|o| match o {
                    Object::Integer(i) => Some(*i as u32),
                    _ => None,
                })
                .or_else(|| self.resolve_inherited_ff(&dict));
            let ff = ff.unwrap_or(0);

            // Determine display text based on field type
            let display_text = match ft.as_deref() {
                Some("Tx") => {
                    // Text field: use /V string value
                    if ff & field_flags::PASSWORD != 0 {
                        // Password field: render as asterisks
                        Some("********".to_string())
                    } else {
                        let value = Self::parse_string_value_static(dict.get("V"))
                            .or_else(|| self.resolve_inherited_field_value(&dict));
                        match value {
                            Some(v) if !v.trim().is_empty() => {
                                // Bound the value to the widget's visual
                                // capacity. Multi-line text-area fields
                                // can hold scrollable content far larger
                                // than the bbox visually renders; per
                                // spec §12.7.4.3 `/V` is the field's
                                // data, but `extract_text` semantics
                                // target what would be visible on the
                                // page. Truncate keeps the rendered
                                // portion and drops the overflow.
                                Some(Self::truncate_to_widget_capacity(
                                    v.trim().to_string(),
                                    &rect,
                                ))
                            }
                            _ => {
                                // Fallback: try AP stream text. Truncate
                                // to bbox capacity — some PDFs reuse a
                                // single Form XObject for many widgets'
                                // `/AP /N`, pointing every widget's
                                // appearance at the page-background
                                // content; without the cap each widget
                                // would extract that content once.
                                self.extract_text_from_ap_stream(&dict).and_then(|t| {
                                    let t = t.trim().to_string();
                                    if t.is_empty() {
                                        return None;
                                    }
                                    Some(Self::truncate_to_widget_capacity(t, &rect))
                                })
                            }
                        }
                    }
                }
                Some("Btn") => {
                    if ff & field_flags::PUSH_BUTTON != 0 {
                        // Push button: caption is in /MK /CA per PDF Spec
                        // ISO 32000-1:2008 §12.5.6.19 (Appearance Characteristics
                        // Dictionary). Extracting it lets screen readers
                        // text-extraction consumers see the button label.
                        dict.get("MK")
                            .and_then(|mk| mk.as_dict())
                            .and_then(|mk| Self::parse_string_value_static(mk.get("CA")))
                            .and_then(|s| {
                                let t = s.trim().to_string();
                                if t.is_empty() {
                                    None
                                } else {
                                    Some(t)
                                }
                            })
                    } else {
                        // Checkbox or radio button
                        let value = Self::parse_string_value_static(dict.get("V"))
                            .or_else(|| self.resolve_inherited_field_value(&dict));
                        let is_checked = match &value {
                            Some(v) => {
                                let v_lower = v.to_ascii_lowercase();
                                v_lower != "off" && !v_lower.is_empty()
                            }
                            None => false,
                        };
                        if is_checked {
                            // A checked box is meaningful state worth surfacing.
                            Some("[x]".to_string())
                        } else {
                            // An UNCHECKED box carries no text. Emitting "[ ]"
                            // here injected noise that pdftotext/PyMuPDF never
                            // produce — the dominant cause of pdf_oxide being
                            // the sole outlier on AcroForm-heavy PDFs in the
                            // cross-corpus sweep (CORPUS-1). Emit nothing.
                            None
                        }
                    }
                }
                Some("Ch") => {
                    // Choice field: use /V selected value
                    let value = dict.get("V");
                    match value {
                        Some(Object::Array(arr)) => {
                            // Multiple selections: join with ", "
                            let items: Vec<String> = arr
                                .iter()
                                .filter_map(|item| Self::parse_string_value_static(Some(item)))
                                .collect();
                            if items.is_empty() {
                                None
                            } else {
                                Some(items.join(", "))
                            }
                        }
                        other => Self::parse_string_value_static(other)
                            .or_else(|| self.resolve_inherited_field_value(&dict))
                            .and_then(|v| {
                                let t = v.trim().to_string();
                                if t.is_empty() {
                                    None
                                } else {
                                    Some(t)
                                }
                            }),
                    }
                }
                Some("Sig") => {
                    // Signature field: skip (no user-visible text)
                    None
                }
                _ => {
                    // Unknown field type: try /V as text
                    Self::parse_string_value_static(dict.get("V"))
                        .or_else(|| self.resolve_inherited_field_value(&dict))
                        .and_then(|v| {
                            let t = v.trim().to_string();
                            if t.is_empty() {
                                None
                            } else {
                                Some(t)
                            }
                        })
                }
            };

            let text = match display_text {
                Some(t) if !t.is_empty() => t,
                _ => {
                    // CORPUS-5: a widget with no extractable /V value (notably a
                    // signature field, /FT /Sig) often carries its VISIBLE text
                    // in the /AP/N appearance stream (e.g. "Firmato
                    // elettronicamente da ..."). pdftotext / PyMuPDF surface it;
                    // fall back to the appearance stream so it isn't dropped.
                    // Fields that DO yield a /V value take the arm above, so this
                    // never double-extracts.
                    match self.extract_text_from_ap_stream(&dict) {
                        Some(ap) if !ap.trim().is_empty() => ap.trim().to_string(),
                        _ => continue,
                    }
                }
            };

            // Parse font size from /DA string
            let font_size = {
                let da = dict
                    .get("DA")
                    .and_then(|o| match o {
                        Object::String(s) => Some(Self::decode_pdf_text_string(s)),
                        _ => None,
                    })
                    .or_else(|| self.resolve_inherited_da(&dict));

                match da {
                    Some(da_str) => {
                        let size = Self::parse_font_size_from_da(&da_str);
                        if size <= 0.0 {
                            // Auto-size: estimate from rect height
                            (rect.height * 0.7).clamp(6.0, 24.0)
                        } else {
                            size
                        }
                    }
                    None => {
                        // No DA at all: estimate from rect height
                        (rect.height * 0.7).clamp(6.0, 24.0)
                    }
                }
            };

            spans.push(TextSpan {
                provenance: None,
                artifact_type: None,
                text,
                bbox: rect,
                font_name: String::new(),
                font_size,
                font_weight: crate::layout::text_block::FontWeight::Normal,
                is_italic: false,
                is_monospace: false,
                color: crate::layout::text_block::Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                },
                mcid: None,
                mcid_scope: None,
                sequence: base_sequence + idx,
                split_boundary_before: false,
                offset_semantic: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                text_rise: 0.0,
                rtl_draw_logical: false,
            });
        }

        spans
    }

    /// Build TextSpan objects from the /Contents field of content-bearing annotations.
    ///
    /// Sticky note (/Subtype/Text), FreeText, Stamp, and markup annotations carry
    /// human-readable text in their /Contents field. Widget annotations are already
    /// handled by `extract_widget_spans`; Popup annotations hold no independent
    /// content (their text belongs to the parent annotation).
    fn annotation_content_spans(&self, page_index: usize) -> Vec<TextSpan> {
        use crate::geometry::Rect;

        let page_obj = match self.get_page(page_index) {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };
        let page_dict = match page_obj.as_dict() {
            Some(d) => d,
            None => return Vec::new(),
        };

        let annots_arr = match page_dict.get("Annots") {
            Some(Object::Array(arr)) => arr.clone(),
            Some(Object::Reference(r)) => match self.load_object(*r) {
                Ok(Object::Array(arr)) => arr,
                _ => return Vec::new(),
            },
            _ => return Vec::new(),
        };

        let mut spans: Vec<TextSpan> = Vec::new();
        let base_sequence = 2_000_000usize; // sort after widget spans

        for (idx, annot_obj) in annots_arr.iter().enumerate() {
            let annot_ref = match annot_obj {
                Object::Reference(r) => *r,
                _ => continue,
            };
            let dict = match self.load_object(annot_ref) {
                Ok(obj) => match obj.as_dict() {
                    Some(d) => d.clone(),
                    None => continue,
                },
                Err(_) => continue,
            };

            let subtype = match dict.get("Subtype").and_then(|s| s.as_name()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let subtype_lc = subtype.to_ascii_lowercase();

            // Skip Widget (handled by extract_widget_spans) and Popup (no independent content).
            if subtype_lc == "widget" || subtype_lc == "popup" {
                continue;
            }

            // Skip invisible / hidden / NoView annotations.
            if let Some(Object::Integer(f)) = dict.get("F") {
                if *f & (0x1 | 0x2 | 0x20) != 0 {
                    continue;
                }
            }

            // Only FreeText and Stamp have /Contents representing visible page text.
            // Text (sticky-note) /Contents is reviewer comment text shown in a pop-up
            // window, not rendered on the page — exclude it to avoid injecting popup
            // notes into the body text stream.
            // For FreeText/Stamp: try /Contents first; fall back to AP stream so that
            // Stamp annotations with empty /Contents but a rendered AP stream are included.
            let is_visible = matches!(subtype_lc.as_str(), "freetext" | "stamp");
            if !is_visible {
                continue;
            }

            let text = {
                let from_contents = if let Some(Object::String(s)) = dict.get("Contents") {
                    let decoded = Self::decode_pdf_text_string(s).trim().to_string();
                    if decoded.is_empty() {
                        None
                    } else {
                        Some(decoded)
                    }
                } else {
                    None
                };
                if let Some(t) = from_contents {
                    t
                } else {
                    match self.extract_text_from_ap_stream(&dict) {
                        Some(ap_text) if !ap_text.trim().is_empty() => ap_text.trim().to_string(),
                        _ => continue,
                    }
                }
            };

            // Use /Rect as the annotation's bounding box.
            // /Rect may be a direct array or an indirect reference to an array.
            let rect_obj = match dict.get("Rect") {
                Some(Object::Reference(r)) => match self.load_object(*r) {
                    Ok(o) => o,
                    Err(_) => continue,
                },
                Some(o) => o.clone(),
                None => continue,
            };
            let rect = match rect_obj.as_array() {
                Some(arr) if arr.len() == 4 => {
                    let mut coords = [0.0f32; 4];
                    let mut ok = true;
                    for (i, item) in arr.iter().enumerate() {
                        match item {
                            Object::Integer(n) => coords[i] = *n as f32,
                            Object::Real(f) => coords[i] = *f as f32,
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if !ok {
                        continue;
                    }
                    let x = coords[0].min(coords[2]);
                    let y = coords[1].min(coords[3]);
                    let w = (coords[2] - coords[0]).abs();
                    let h = (coords[3] - coords[1]).abs();
                    Rect {
                        x,
                        y,
                        width: w.max(1.0),
                        height: h.max(1.0),
                    }
                }
                _ => continue,
            };

            spans.push(TextSpan {
                provenance: None,
                artifact_type: None,
                text,
                bbox: rect,
                font_name: String::new(),
                font_size: 12.0,
                font_weight: crate::layout::text_block::FontWeight::Normal,
                is_italic: false,
                is_monospace: false,
                color: crate::layout::text_block::Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                },
                mcid: None,
                mcid_scope: None,
                sequence: base_sequence + idx,
                split_boundary_before: false,
                offset_semantic: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                text_rise: 0.0,
                rtl_draw_logical: false,
            });
        }

        spans
    }

    /// Walk /Parent chain to find inherited /Ff (field flags) value.
    fn resolve_inherited_ff(
        &self,
        dict: &std::collections::HashMap<String, Object>,
    ) -> Option<u32> {
        let mut parent_ref = match dict.get("Parent") {
            Some(Object::Reference(r)) => Some(*r),
            _ => return None,
        };
        let mut depth = 0;
        while let Some(pref) = parent_ref {
            if depth >= 10 {
                break;
            }
            depth += 1;
            if let Ok(parent_obj) = self.load_object(pref) {
                if let Some(parent_dict) = parent_obj.as_dict() {
                    if let Some(Object::Integer(ff)) = parent_dict.get("Ff") {
                        return Some(*ff as u32);
                    }
                    parent_ref = match parent_dict.get("Parent") {
                        Some(Object::Reference(r)) => Some(*r),
                        _ => None,
                    };
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        None
    }

    /// Walk /Parent chain (and AcroForm) to find inherited /DA (Default Appearance) string.
    fn resolve_inherited_da(
        &self,
        dict: &std::collections::HashMap<String, Object>,
    ) -> Option<String> {
        // First check parent chain
        let mut parent_ref = match dict.get("Parent") {
            Some(Object::Reference(r)) => Some(*r),
            _ => None,
        };
        let mut depth = 0;
        while let Some(pref) = parent_ref {
            if depth >= 10 {
                break;
            }
            depth += 1;
            if let Ok(parent_obj) = self.load_object(pref) {
                if let Some(parent_dict) = parent_obj.as_dict() {
                    if let Some(Object::String(da)) = parent_dict.get("DA") {
                        return Some(Self::decode_pdf_text_string(da));
                    }
                    parent_ref = match parent_dict.get("Parent") {
                        Some(Object::Reference(r)) => Some(*r),
                        _ => None,
                    };
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        // Fall back to AcroForm-level /DA
        if let Some(trailer_dict) = self.trailer.as_dict() {
            if let Some(root_ref) = trailer_dict.get("Root").and_then(|o| o.as_reference()) {
                if let Ok(root_obj) = self.load_object(root_ref) {
                    if let Some(root_dict) = root_obj.as_dict() {
                        let acroform = match root_dict.get("AcroForm") {
                            Some(Object::Reference(r)) => self.load_object(*r).ok(),
                            Some(obj) => Some(obj.clone()),
                            None => None,
                        };
                        if let Some(acroform_obj) = acroform {
                            if let Some(af_dict) = acroform_obj.as_dict() {
                                if let Some(Object::String(da)) = af_dict.get("DA") {
                                    return Some(Self::decode_pdf_text_string(da));
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }

    /// Append text from non-widget annotations on a page.
    ///
    /// Extracts text from FreeText annotations (text box contents), Stamp annotations
    /// (appearance stream text), and other non-widget annotation types.
    /// Widget annotations are handled separately via `extract_widget_spans()`.
    /// Skips hidden and invisible annotations per PDF spec flags.
    fn append_non_widget_annotation_text(&self, page_index: usize, text: &mut String) {
        // Lightweight annotation text extraction — avoids full get_annotations() overhead.
        // Only reads /Subtype, /V, /Contents, /F, and /Parent (for field value inheritance).
        // Uses get_page() which is cached after first access.
        let page_obj = match self.get_page(page_index) {
            Ok(o) => o,
            Err(_) => return,
        };
        let page_dict = match page_obj.as_dict() {
            Some(d) => d,
            None => return,
        };

        // Get /Annots array (may be direct or indirect)
        let annots_arr = match page_dict.get("Annots") {
            Some(Object::Array(arr)) => arr.clone(),
            Some(Object::Reference(r)) => match self.load_object(*r) {
                Ok(Object::Array(arr)) => arr,
                _ => return,
            },
            _ => return, // No annotations on this page
        };

        let mut annot_texts: Vec<String> = Vec::new();

        for annot_obj in &annots_arr {
            let len_before_annot = annot_texts.len();
            let annot_ref = match annot_obj {
                Object::Reference(r) => *r,
                _ => continue,
            };
            let dict = match self.load_object(annot_ref) {
                Ok(obj) => match obj.as_dict() {
                    Some(d) => d.clone(),
                    None => continue,
                },
                Err(_) => continue,
            };

            // Check /F flags — skip invisible/hidden annotations
            // Bit 1 (0x1) = Invisible, Bit 2 (0x2) = Hidden, Bit 6 (0x20) = NoView
            if let Some(Object::Integer(f)) = dict.get("F") {
                if *f & (0x1 | 0x2 | 0x20) != 0 {
                    continue;
                }
            }

            let subtype = match dict.get("Subtype").and_then(|s| s.as_name()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let subtype_lower = subtype.to_ascii_lowercase();

            match subtype_lower.as_str() {
                "widget" => {
                    // Widgets are now handled by extract_widget_spans() as inline TextSpans.
                    // Skip them here to avoid duplicate text at the end of output.
                    continue;
                }
                "freetext" | "stamp" => {
                    if let Some(Object::String(s)) = dict.get("Contents") {
                        let decoded = Self::decode_pdf_text_string(s);
                        let trimmed = decoded.trim().to_string();
                        if !trimmed.is_empty() {
                            annot_texts.push(trimmed);
                        }
                    }
                }
                // Text (sticky-note) /Contents is reviewer popup comment text, not
                // visible page content — skip to avoid injecting popup notes.
                "text" => {}
                // Geometric shape annotations — per §12.5.6.2, their /Contents is
                // also popup/comment text, same as the markup group below.
                "line" | "circle" | "square" | "polygon" | "polyline" => {
                    // Skip — /Contents is popup comment text, not page content.
                }
                // Markup/comment annotations — per ISO 32000-1 §12.5.6.2 (Table 166),
                // the /Contents of all these subtypes is popup/comment text written
                // by a reviewer, NOT text displayed on the page. Exclude to avoid
                // injecting user annotation notes into the body text stream.
                // Per §12.5.6.2, all of these annotations' /Contents is popup/comment
                // text (displayed in a pop-up window), not rendered page content.
                // FileAttachment is explicitly in this category per §12.5.6.2 even
                // though §12.5.6.15 calls it "descriptive text" — the pop-up semantics
                // take precedence.
                "highlight" | "underline" | "strikeout" | "squiggly" | "caret"
                | "fileattachment" | "redact" | "ink" => {
                    // Skip — /Contents is popup comment text, not page content.
                }
                // Link /Contents is an accessibility alternate description (§12.5.6.5).
                // Treated as supplementary text on pages with no body content.
                "link" => {
                    if let Some(Object::String(s)) = dict.get("Contents") {
                        let decoded = Self::decode_pdf_text_string(s);
                        let trimmed = decoded.trim().to_string();
                        if !trimmed.is_empty() {
                            annot_texts.push(trimmed);
                        }
                    }
                }
                // Popup annotations — per §12.5.6.14 Table 183, the parent
                // annotation's /Contents overrides the popup's own /Contents.
                "popup" => {
                    // Try parent annotation's /Contents first (spec §12.5.6.14).
                    let mut got_text = false;
                    if let Some(parent_ref) = dict.get("Parent").and_then(|o| o.as_reference()) {
                        if let Ok(parent_obj) = self.load_object(parent_ref) {
                            if let Some(parent_dict) = parent_obj.as_dict() {
                                if let Some(Object::String(s)) = parent_dict.get("Contents") {
                                    let decoded = Self::decode_pdf_text_string(s);
                                    let trimmed = decoded.trim().to_string();
                                    if !trimmed.is_empty() {
                                        annot_texts.push(trimmed);
                                        got_text = true;
                                    }
                                }
                            }
                        }
                    }
                    // Fall back to the popup's own /Contents only when parent has none.
                    if !got_text {
                        if let Some(Object::String(s)) = dict.get("Contents") {
                            let decoded = Self::decode_pdf_text_string(s);
                            let trimmed = decoded.trim().to_string();
                            if !trimmed.is_empty() {
                                annot_texts.push(trimmed);
                            }
                        }
                    }
                }
                _ => {
                    // For any other annotation type, also try /Contents
                    if let Some(Object::String(s)) = dict.get("Contents") {
                        let decoded = Self::decode_pdf_text_string(s);
                        let trimmed = decoded.trim().to_string();
                        if !trimmed.is_empty() {
                            annot_texts.push(trimmed);
                        }
                    }
                }
            }

            // Fallback: if no text was extracted from /V or /Contents,
            // try extracting from the /AP/N (Normal Appearance) stream.
            let text_before = annot_texts.len();
            if text_before == len_before_annot {
                if let Some(ap_text) = self.extract_text_from_ap_stream(&dict) {
                    let trimmed = ap_text.trim().to_string();
                    if !trimmed.is_empty() {
                        annot_texts.push(trimmed);
                    }
                }
            }
        }

        if !annot_texts.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&annot_texts.join("\n"));
        }
    }

    /// Extract text from an annotation's Normal Appearance stream (/AP/N).
    ///
    /// AP streams are content streams with their own /Resources. This creates
    /// a temporary TextExtractor, loads fonts from the AP stream resources,
    /// and extracts text spans from the decoded stream data.
    fn extract_text_from_ap_stream(
        &self,
        annot_dict: &std::collections::HashMap<String, Object>,
    ) -> Option<String> {
        use crate::extractors::TextExtractor;

        // Get /AP dictionary
        let ap_obj = annot_dict.get("AP")?;
        let ap = if let Some(r) = ap_obj.as_reference() {
            self.load_object(r).ok()?
        } else {
            ap_obj.clone()
        };
        let ap_dict = ap.as_dict()?;

        // Get /N (Normal appearance) — can be a stream ref or a dictionary of states
        let n_obj = ap_dict.get("N")?;
        let (n_stream, n_ref) = match n_obj {
            Object::Reference(r) => (self.load_object(*r).ok()?, *r),
            _ => return None, // N must be a reference to a stream
        };

        // Verify it's a stream (has a dict with stream data)
        let n_dict = n_stream.as_dict()?;

        // Decode the AP/N stream
        let stream_data = match self.decode_stream_with_encryption(&n_stream, n_ref) {
            Ok(data) => data,
            Err(_) => return None,
        };

        // Quick check: does the stream contain text operators?
        if !Self::may_contain_text(&stream_data) {
            return None;
        }

        // Create a temporary text extractor for this AP stream
        let mut extractor = TextExtractor::new();

        // Load fonts from the AP/N stream's own /Resources. No resources on
        // the AP stream — try the annotation's /DR or parent page resources
        // — means we can't decode fonts, so bail.
        {
            let resources = n_dict.get("Resources")?;
            let res_obj = if let Some(r) = resources.as_reference() {
                self.load_object(r)
                    .ok()
                    .unwrap_or_else(|| resources.clone())
            } else {
                resources.clone()
            };
            extractor.set_resources(res_obj.clone());
            extractor.set_document(self);
            let _ = self.load_fonts(&res_obj, &mut extractor);
        }

        // Extract text spans from the AP stream
        let spans = extractor.extract_text_spans(&stream_data).ok()?;
        if spans.is_empty() {
            return None;
        }

        // Collect span text
        let text: String = spans
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if text.trim().is_empty() {
            return None;
        }
        Some(text)
    }

    /// Char-count capacity for what physically fits inside a widget
    /// bbox at body font sizes. Per PDF spec §12.7.4.3 the field's
    /// value is `/V`; the appearance stream is visual rendering
    /// only. When we fall back to AP extraction the result must be
    /// bounded by what the widget could visually show — PDFs that
    /// reuse a single Form XObject for many widgets' `/AP /N` would
    /// otherwise dump the shared content once per widget, and
    /// scrollable multi-line text fields hold far more characters
    /// in `/V` than ever render at once.
    ///
    /// Heuristic: ~14 chars per cm² at body font sizes. At PDF
    /// 72 dpi (1 pt = 0.0353 cm), the formula
    /// `capacity = 0.0175 * w_pt * h_pt + 64` applies; the constant
    /// term absorbs short labels where the area estimate alone is
    /// too tight to even hold the field's name.
    fn widget_text_capacity(bbox: &crate::geometry::Rect) -> usize {
        let area = bbox.width.max(0.0) * bbox.height.max(0.0);
        (0.0175 * area) as usize + 64
    }

    /// Truncate `text` to the widget's visual capacity. If `text`
    /// already fits, returns it unchanged. Used to bound AP-fallback
    /// extraction (and other content paths) so a single widget can't
    /// dump page-background prose or scrollable field internals into
    /// the page text.
    fn truncate_to_widget_capacity(text: String, bbox: &crate::geometry::Rect) -> String {
        let cap = Self::widget_text_capacity(bbox);
        let n = text.chars().count();
        if n <= cap {
            return text;
        }
        text.chars().take(cap).collect()
    }

    /// Walk /Parent chain to find inherited /FT (field type) value.
    fn resolve_inherited_ft(
        &self,
        dict: &std::collections::HashMap<String, Object>,
    ) -> Option<String> {
        let mut parent_ref = match dict.get("Parent") {
            Some(Object::Reference(r)) => Some(*r),
            _ => return None,
        };
        let mut depth = 0;
        while let Some(pref) = parent_ref {
            if depth >= 10 {
                break;
            }
            depth += 1;
            if let Ok(parent_obj) = self.load_object(pref) {
                if let Some(parent_dict) = parent_obj.as_dict() {
                    if let Some(ft) = parent_dict.get("FT").and_then(|o| o.as_name()) {
                        return Some(ft.to_string());
                    }
                    parent_ref = match parent_dict.get("Parent") {
                        Some(Object::Reference(r)) => Some(*r),
                        _ => None,
                    };
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        None
    }

    /// Walk /Parent chain to find inherited /V value (PDF spec 12.7.3.1).
    fn resolve_inherited_field_value(
        &self,
        dict: &std::collections::HashMap<String, Object>,
    ) -> Option<String> {
        let mut parent_ref = match dict.get("Parent") {
            Some(Object::Reference(r)) => Some(*r),
            _ => return None,
        };
        let mut depth = 0;
        while let Some(pref) = parent_ref {
            if depth >= 10 {
                break;
            }
            depth += 1;
            if let Ok(parent_obj) = self.load_object(pref) {
                if let Some(parent_dict) = parent_obj.as_dict() {
                    if let Some(v) = Self::parse_string_value_static(parent_dict.get("V")) {
                        return Some(v);
                    }
                    parent_ref = match parent_dict.get("Parent") {
                        Some(Object::Reference(r)) => Some(*r),
                        _ => None,
                    };
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        None
    }

    /// Parse a string value from a PDF object with proper PDF string decoding.
    /// Handles UTF-16BE (BOM \xFE\xFF) and PDFDocEncoding per ISO 32000-1 §7.9.2.2.
    fn parse_string_value_static(obj: Option<&Object>) -> Option<String> {
        match obj {
            Some(Object::String(s)) => Some(Self::decode_pdf_text_string(s)),
            Some(Object::Name(n)) => Some(n.clone()),
            Some(Object::Integer(i)) => Some(i.to_string()),
            Some(Object::Real(f)) => Some(f.to_string()),
            _ => None,
        }
    }

    /// Decode a PDF text string that may be UTF-16BE/LE (with BOM) or PDFDocEncoding.
    fn decode_pdf_text_string(bytes: &[u8]) -> String {
        if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
            // UTF-16BE with BOM
            let utf16_bytes = &bytes[2..];
            let utf16_pairs: Vec<u16> = utf16_bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                .collect();
            String::from_utf16(&utf16_pairs)
                .unwrap_or_else(|_| String::from_utf8_lossy(bytes).to_string())
        } else if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
            // UTF-16LE with BOM
            let utf16_bytes = &bytes[2..];
            let utf16_pairs: Vec<u16> = utf16_bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            String::from_utf16(&utf16_pairs)
                .unwrap_or_else(|_| String::from_utf8_lossy(bytes).to_string())
        } else {
            // PDFDocEncoding — superset of ISO Latin-1
            bytes
                .iter()
                .filter_map(|&b| crate::fonts::font_dict::pdfdoc_encoding_lookup(b))
                .collect()
        }
    }

    /// Check if decoded content stream data may contain text.
    ///
    /// Returns true if the stream contains either:
    /// - A BT (Begin Text) operator (text is directly in the page stream)
    /// - A Do operator (Form XObject invocation that may contain text)
    ///
    /// Per §9.4.3, text-showing operators shall only appear within BT...ET text
    /// objects. However, a page may contain text only inside Form XObjects
    /// referenced via `Do` operators, so we must also check for those.
    pub(crate) fn may_contain_text(data: &[u8]) -> bool {
        // SIMD-accelerated pre-check using memchr to find candidate positions
        // for BT (Begin Text) and Do (XObject invocation) operators.
        // ~50x faster than byte-by-byte scanning for large graphics-heavy pages.
        fn is_boundary(b: u8) -> bool {
            b.is_ascii_whitespace()
                || matches!(
                    b,
                    b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
                )
        }

        // Search for 'B' (BT) and 'D' (Do) candidates using SIMD memchr
        let len = data.len();
        let mut offset = 0;
        while offset + 1 < len {
            // Find next 'B' or 'D' byte
            match memchr::memchr2(b'B', b'D', &data[offset..]) {
                None => return false,
                Some(pos) => {
                    let i = offset + pos;
                    if i + 1 >= len {
                        return false;
                    }
                    // Check for BT operator
                    if data[i] == b'B' && data[i + 1] == b'T' {
                        let before_ok = i == 0 || is_boundary(data[i - 1]);
                        let after_ok = i + 2 >= len || is_boundary(data[i + 2]);
                        if before_ok && after_ok {
                            return true;
                        }
                    }
                    // Check for Do operator
                    if data[i] == b'D' && data[i + 1] == b'o' {
                        let before_ok = i == 0 || is_boundary(data[i - 1]);
                        let after_ok = i + 2 >= len || is_boundary(data[i + 2]);
                        if before_ok && after_ok {
                            return true;
                        }
                    }
                    offset = i + 1;
                }
            }
        }
        false
    }

    /// Check if a page definitely cannot produce any text based on its resources.
    ///
    /// Returns `true` if the page has no `/Font` resources and no Form XObjects
    /// (which could contain nested text). This allows skipping content stream
    /// decompression and parsing entirely for image-only/scanned pages.
    ///
    /// Returns `false` (conservative) if resources can't be inspected.
    fn page_cannot_have_text(&self, page_dict: &HashMap<String, Object>) -> bool {
        let resources = match page_dict.get("Resources") {
            Some(r) => {
                if let Some(ref_obj) = r.as_reference() {
                    match self.load_object(ref_obj) {
                        Ok(obj) => obj,
                        Err(_) => return false, // Can't resolve — be conservative
                    }
                } else {
                    r.clone()
                }
            }
            None => return true, // No resources at all → no text possible
        };

        let res_dict = match resources.as_dict() {
            Some(d) => d,
            None => return false,
        };

        // If the page has any /Font resources, it might produce text
        if let Some(font_obj) = res_dict.get("Font") {
            let font_dict = if let Some(ref_obj) = font_obj.as_reference() {
                self.load_object(ref_obj).ok()
            } else {
                Some(font_obj.clone())
            };
            if let Some(fd) = font_dict {
                if let Some(d) = fd.as_dict() {
                    if !d.is_empty() {
                        return false; // Has fonts → might have text
                    }
                }
            }
        }

        // Check XObjects: if any are Form type, they could contain nested text.
        // Uses lightweight is_form_xobject() peek instead of full load_object()
        // to avoid expensive I/O for image-heavy PDFs (e.g., Deutsche: 375MB images).
        if let Some(xobj_obj) = res_dict.get("XObject") {
            let xobj_dict_obj = if let Some(ref_obj) = xobj_obj.as_reference() {
                self.load_object(ref_obj).ok()
            } else {
                Some(xobj_obj.clone())
            };
            if let Some(xobj_dict_resolved) = xobj_dict_obj {
                if let Some(xobj_dict) = xobj_dict_resolved.as_dict() {
                    for xobj_ref in xobj_dict.values() {
                        if let Some(ref_obj) = xobj_ref.as_reference() {
                            // Use lightweight 1KB peek instead of full object load
                            if self.is_form_xobject(ref_obj) {
                                return false; // Form XObject could contain text
                            }
                        } else if let Some(d) = xobj_ref.as_dict() {
                            if d.get("Subtype").and_then(|s| s.as_name()) == Some("Form") {
                                return false;
                            }
                        }
                    }
                }
            }
        }

        // No fonts and no Form XObjects → page is image-only
        true
    }

    /// Assemble the page's text spans via the reading-order
    /// pipeline, classifying each region with the per-class
    /// detectors in [`crate::pipeline::reading_order::detectors`].
    /// Returns the assembled spans plus the detector class that
    /// fired on each region.
    ///
    /// The four detectors handle layout shapes that the plain
    /// y-then-x assembly cannot produce correctly:
    ///
    /// - **DramaticScript**: Macbeth-style speaker-tag layouts —
    ///   row-major join required.
    /// - **DenseSingleLine**: SEC DEF 14A 8pt-body interleave —
    ///   single-row regroup required.
    /// - **SubSuperBaselineReattach**: chemical-formula
    ///   subscripts — baseline reattach required.
    /// - **NarrowTrackedJustified**: stretched justified columns —
    ///   per-line median-gap threshold normalisation required.
    ///
    /// Regions that don't match any specific layout fall through to
    /// `Default` (plain y-then-x assembly within the block).
    ///
    /// Callers can use this as a pre-step before applying their own
    /// assembly logic, or rely on the classified `ReadingOrderClass`
    /// to dispatch their assembly strategy. `extract_text` consumes
    /// this implicitly through `extract_spans` + the existing
    /// `XYCutStrategy`.
    pub fn assemble_text_via_reading_order(
        &self,
        page_index: usize,
    ) -> Result<(
        Vec<crate::layout::TextSpan>,
        crate::pipeline::reading_order::ReadingOrderClass,
    )> {
        if self.is_encrypted_unreadable() {
            log::warn!("PDF is encrypted and could not be decrypted; returning empty text");
            return Ok((
                Vec::new(),
                crate::pipeline::reading_order::ReadingOrderClass::Default,
            ));
        }
        let spans = self.extract_spans(page_index)?;
        // Convert spans to detector input. We only need the geometric
        // signal (x/y/width/font_size), not the full TextSpan
        // semantics.
        let glyphs: Vec<crate::pipeline::reading_order::DetectorGlyph> = spans
            .iter()
            .map(|s| crate::pipeline::reading_order::DetectorGlyph {
                x: s.bbox.x,
                y: s.bbox.y,
                width: s.bbox.width,
                font_size: s.font_size,
                text_len: s.text.chars().count(),
            })
            .collect();
        // Build per-row text strings for DramaticScript detector,
        // together with the leftmost glyph of each row (for the X-
        // consistency check). Group spans by Y (within 0.5 pt),
        // concatenating their texts in the order they appear in
        // `spans` and tracking the smallest X seen per row.
        let mut rows: Vec<(f32, String, crate::pipeline::reading_order::DetectorGlyph)> =
            Vec::new();
        for span in &spans {
            let span_glyph = crate::pipeline::reading_order::DetectorGlyph {
                x: span.bbox.x,
                y: span.bbox.y,
                width: span.bbox.width,
                font_size: span.font_size,
                text_len: span.text.chars().count(),
            };
            let mut placed = false;
            for (y, text, first) in rows.iter_mut() {
                if (*y - span.bbox.y).abs() < 0.5 {
                    text.push(' ');
                    text.push_str(&span.text);
                    if span_glyph.x < first.x {
                        *first = span_glyph;
                    }
                    placed = true;
                    break;
                }
            }
            if !placed {
                rows.push((span.bbox.y, span.text.clone(), span_glyph));
            }
        }
        let row_texts: Vec<&str> = rows.iter().map(|(_, t, _)| t.as_str()).collect();
        let row_first_glyphs: Vec<crate::pipeline::reading_order::DetectorGlyph> =
            rows.iter().map(|(_, _, g)| *g).collect();
        let class =
            crate::pipeline::reading_order::classify_region(&glyphs, &row_first_glyphs, &row_texts);
        Ok((spans, class))
    }

    /// Returns `true` if the page has any text-bearing content (fonts in
    /// resources + at least one `BT`/`Do` operator in the content stream),
    /// `false` if the page is image-only or genuinely empty.
    ///
    /// Callers can route image-only pages to raster rendering instead of
    /// receiving an empty string with no signal.
    ///
    /// Conservative: returns `true` when the page resources can't be
    /// inspected (load error, encrypted-not-authenticated, etc.) so the
    /// caller still attempts extraction.
    ///
    /// # PDF spec basis
    ///
    /// §8.8 (Image XObjects): image-only pages have `/Resources` whose
    /// only `/XObject` entries are `/Subtype /Image` with no `/Font`
    /// resources.
    pub fn has_text_layer(&self, page_index: usize) -> Result<bool> {
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;
        if self.page_cannot_have_text(page_dict) {
            return Ok(false);
        }
        // Probe content stream for text-showing operators. If we can't
        // read the content stream, be conservative and say yes (let
        // extraction try).
        match self.get_page_content_data(page_index) {
            Ok(content_data) => Ok(Self::may_contain_text(&content_data)),
            Err(_) => Ok(true),
        }
    }

    /// Returns the document's `/P` permission flags as a `PdfPermissions`
    /// struct if the document is encrypted; `None` otherwise.
    ///
    /// Per PDF spec §7.6.3.2 the `/P` flag is advisory — pdf_oxide
    /// does not enforce restrictions — but callers who want to
    /// enforce them (e.g., refuse copy-protected PDF extraction) can
    /// do so themselves by checking the returned permissions.
    ///
    /// # PDF spec basis
    ///
    /// §7.6.3.2 Table 22 (`/P` Standard Encryption Dictionary entry).
    /// Decoding is implemented in `encryption::permissions::PdfPermissions::from_p_flag`.
    pub fn permissions(&self) -> Option<crate::encryption::PdfPermissions> {
        // ensure_encryption_initialized may fail on malformed Encrypt
        // dicts — that's fine, no permissions surface for those.
        let _ = self.ensure_encryption_initialized();
        let handler = self.encryption_handler.lock_or_recover();
        let handler = handler.as_ref()?;
        Some(crate::encryption::PdfPermissions::from_p_flag(
            handler.raw_permissions(),
        ))
    }

    /// Extract text using structure tree for Tagged PDFs.
    ///
    /// This method implements PDF spec-compliant text extraction for Tagged PDFs
    /// using the logical structure tree to determine reading order.
    ///
    /// # PDF Spec Reference
    ///
    /// ISO 32000-1:2008 Section 14.8.2.3 - Determining the Text Extraction Sequence
    /// "For a Tagged PDF document, conforming readers shall present the document's
    /// content to the user in the order given by a pre-order traversal of the
    /// structure hierarchy"
    ///
    /// # Algorithm
    /// 1. Extract all text spans with MCIDs from the page
    /// 2. Build a map from MCID → Vec<TextSpan>
    /// 3. Traverse structure tree in pre-order to get MCIDs in reading order
    /// 4. Assemble text by looking up spans for each MCID in order
    ///
    /// # Arguments
    /// * `page_index` - Zero-based page index
    /// * `struct_tree` - The structure tree root from the PDF catalog
    ///
    /// # Returns
    /// Extracted text in logical structure order
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // This is called automatically by extract_text() for Tagged PDFs
    /// let text = doc.extract_text(0)?;
    /// ```
    #[allow(dead_code)]
    fn extract_text_structure_order(
        &self,
        page_index: usize,
        struct_tree: &crate::structure::StructTreeRoot,
    ) -> Result<String> {
        log::debug!(
            "Extracting text using structure tree for page {}",
            page_index
        );

        // Step 1: Extract all spans with MCIDs
        let all_spans = self.extract_spans(page_index)?;

        if all_spans.is_empty() {
            let mut text = String::new();
            self.append_non_widget_annotation_text(page_index, &mut text);
            return Ok(text);
        }

        // Step 2: Build MCID → Vec<TextSpan> map
        let mut mcid_map: HashMap<u32, Vec<TextSpan>> = HashMap::new();
        let mut spans_without_mcid: Vec<TextSpan> = Vec::new();

        for span in all_spans {
            if let Some(mcid) = span.mcid {
                mcid_map.entry(mcid).or_default().push(span);
            } else {
                // Collect spans without MCID (shouldn't happen in well-formed Tagged PDFs)
                spans_without_mcid.push(span);
            }
        }

        log::debug!(
            "Found {} MCIDs with spans, {} spans without MCID",
            mcid_map.len(),
            spans_without_mcid.len()
        );

        // Step 3: Traverse structure tree to get MCIDs in reading order
        let ordered_content = traverse_structure_tree(struct_tree, page_index as u32)
            .map_err(|e| Error::InvalidPdf(format!("Failed to traverse structure tree: {}", e)))?;

        log::debug!(
            "Structure tree traversal found {} content items in reading order",
            ordered_content.len()
        );

        // Resolve struct-tree-scope `/ActualText`. The mcid-driven
        // emission walk consults the cached index and assigns at most
        // one action per MCID — either "emit the replacement and
        // suppress this MCID's raw glyphs" or "suppress only".
        let at_index = self.actualtext_index();
        let mc_wins: HashSet<u32> = self
            .mc_actualtext_mcids
            .lock_or_recover()
            .get(&page_index)
            .cloned()
            .unwrap_or_default();
        let default_scope = crate::structure::McidScope::Page(page_index as u32);
        let mcid_order: Vec<(crate::structure::McidScope, u32)> = ordered_content
            .iter()
            .filter_map(|c| {
                c.mcid
                    .map(|m| (c.mcid_scope.clone().unwrap_or(default_scope.clone()), m))
            })
            .collect();
        // Per-key rendered glyph text for the §14.9.4 conformance gate.
        let mut glyph_text: HashMap<(crate::structure::McidScope, u32), String> = HashMap::new();
        for (scope, m) in &mcid_order {
            if let Some(sp) = mcid_map.get(m) {
                let joined: String = sp.iter().map(|s| s.text.as_str()).collect();
                glyph_text
                    .entry((scope.clone(), *m))
                    .or_default()
                    .push_str(&joined);
            }
        }
        let actions = Self::actualtext_actions_for_page(
            at_index.as_deref(),
            &mcid_order,
            |_scope, m| mcid_map.contains_key(&m),
            &mc_wins,
            &glyph_text,
        );

        // Step 4: Assemble text in structure order
        let mut text = String::with_capacity(mcid_map.len() * 50); // estimate
        let mut prev_span: Option<TextSpan> = None;
        // Whether the content element that emitted `prev_span` sat inside a
        // table — used to collapse a table row boundary to a single newline.
        let mut prev_in_table = false;
        let mut consumed_mcids: std::collections::HashSet<u32> = std::collections::HashSet::new();

        for content in &ordered_content {
            // Handle word break markers by inserting a space
            if content.is_word_break {
                if !text.is_empty() && !text.ends_with(' ') && !text.ends_with('\n') {
                    text.push(' ');
                }
                continue;
            }

            // For regular content with MCID
            let Some(mcid) = content.mcid else {
                continue;
            };
            // ISO 32000-1 §14.7: a marked-content sequence's MCID is unique
            // within its content stream and is referenced from the structure
            // hierarchy at most once. A malformed struct tree that re-references
            // the same MCID multiple times would otherwise emit that MCID's
            // glyphs once per reference. Emit each MCID once. (A destructive
            // /ActualText replacement — now declined by the §14.9.4 conformance
            // gate — can mask this by collapsing each consecutive run to a
            // single emit.)
            if !consumed_mcids.insert(mcid) {
                continue;
            }
            let mcid_scope_key = content.mcid_scope.clone().unwrap_or(default_scope.clone());

            // ActualText action dispatch. `EmitAndSuppress` is set only
            // on the first visible covered MCID of a consecutive-same-
            // replacement run; subsequent MCIDs in the run carry
            // `Suppress`. MC-scope-wins MCIDs (their BDC carried inline
            // /ActualText) are exempt and walk the raw-span path so
            // the extractor's in-stream replacement reaches output.
            match actions.get(&(mcid_scope_key, mcid)) {
                Some(ActualTextAction::EmitAndSuppress(repl)) => {
                    consumed_mcids.insert(mcid);
                    if !text.is_empty() && !text.ends_with(' ') && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(repl);
                    continue;
                }
                Some(ActualTextAction::Suppress) => {
                    consumed_mcids.insert(mcid);
                    continue;
                }
                None => {}
            }

            if let Some(spans) = mcid_map.get(&mcid) {
                consumed_mcids.insert(mcid);
                let rtl_run = Self::mcid_run_is_pure_rtl(spans);
                // Repair the cross-span Arabic glyph-interleave defect (zero-width
                // mark/consonant spans landing at word edges) before ordering.
                let merged_rtl = Self::merge_interleaved_rtl_lines(spans);
                let use_spans: &[crate::layout::TextSpan] = merged_rtl.as_deref().unwrap_or(spans);
                for span in Self::order_mcid_spans(use_spans) {
                    if let Some(prev) = &prev_span {
                        let y_diff = (prev.bbox.y - span.bbox.y).abs();

                        if y_diff > Self::same_line_threshold(prev, span) {
                            Self::push_line_breaks(
                                &mut text,
                                prev,
                                span,
                                y_diff,
                                content.in_table && prev_in_table,
                            );
                        } else if Self::should_insert_space(prev, span)
                            || Self::stacked_cell_needs_space(prev, span)
                        {
                            text.push(' ');
                        }
                    }

                    Self::push_span_text_bidi(&mut text, span, rtl_run);
                    prev_span = Some(span.clone());
                    prev_in_table = content.in_table;
                }
            } else {
                log::warn!(
                    "Structure tree references MCID {} but no spans found with that MCID",
                    mcid
                );
                self.push_warning(format!(
                    "page {page_index}: structure tree references MCID {mcid} but no content spans found — some text may be missing"
                ));
            }
        }

        // Append spans with MCIDs not referenced by the structure tree.
        // This happens with Form XObjects that lack /StructParents, where
        // their BDC/MCID markers exist in the content stream but are not
        // registered in the page's ParentTree.
        let mut unconsumed: Vec<(&u32, &Vec<TextSpan>)> = mcid_map
            .iter()
            .filter(|(mcid, _)| !consumed_mcids.contains(mcid))
            .collect();
        unconsumed.sort_by_key(|(mcid, _)| **mcid);
        if !unconsumed.is_empty() {
            log::debug!(
                "Appending {} unreferenced MCIDs (e.g., from Form XObjects without StructParents)",
                unconsumed.len()
            );
            for (_mcid, spans) in &unconsumed {
                let rtl_run = Self::mcid_run_is_pure_rtl(spans);
                for span in *spans {
                    if let Some(prev) = &prev_span {
                        let y_diff = (prev.bbox.y - span.bbox.y).abs();
                        if y_diff > Self::same_line_threshold(prev, span) {
                            text.push('\n');
                        } else if Self::should_insert_space(prev, span) {
                            text.push(' ');
                        }
                    }
                    Self::push_span_text_bidi(&mut text, span, rtl_run);
                    prev_span = Some(span.clone());
                }
            }
        }

        // Append any spans without MCID at the end (shouldn't happen in well-formed PDFs)
        if !spans_without_mcid.is_empty() {
            log::warn!(
                "Found {} text spans without MCID - appending to end",
                spans_without_mcid.len()
            );
            for span in &spans_without_mcid {
                if let Some(prev) = &prev_span {
                    let y_diff = (prev.bbox.y - span.bbox.y).abs();
                    if y_diff > Self::same_line_threshold(prev, span) {
                        text.push('\n');
                    } else if Self::should_insert_space(prev, span) {
                        text.push(' ');
                    }
                }
                Self::push_span_text_bidi(&mut text, span, false);
                prev_span = Some(span.clone());
            }
        }

        // Annotation text is already included via annotation_content_spans() in
        // extract_spans() — do NOT call append_non_widget_annotation_text() here
        // (would cause double-emission of all annotation text).

        Ok(text)
    }

    /// Order one MCID's spans for emission in the structure-order assemblers
    ///. A single marked-content element can carry spans across several
    /// visual lines; emitting them in raw extraction order can mis-order them,
    /// so sort by the canonical reading-order comparator. Skipped for single-
    /// span MCIDs and for any MCID containing RTL text (whose span order is
    /// handled by the bidi passes) — both stay byte-identical.
    fn order_mcid_spans(spans: &[crate::layout::TextSpan]) -> Vec<&crate::layout::TextSpan> {
        use crate::text::rtl_detector::is_rtl_text;
        let mut ordered: Vec<&crate::layout::TextSpan> = spans.iter().collect();
        if spans.len() <= 1 {
            return ordered;
        }
        let has_rtl = spans
            .iter()
            .any(|s| s.text.chars().any(|c| is_rtl_text(c as u32)));
        let has_latin = spans
            .iter()
            .any(|s| s.text.chars().any(|c| c.is_ascii_alphabetic()));
        if !has_rtl {
            // LTR multi-span MCID: left-to-right row-aware reading order.
            ordered.sort_by(|a, b| {
                crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
            });
        } else if !has_latin {
            // #656/#657: pure-RTL MCID. The tagged struct-tree path never
            // reaches `reverse_rtl_visual_order_runs`, so without an explicit
            // span-order pass the words emerge in visual (reversed) sequence.
            // Emitting each row right-to-left (X descending) reconstructs
            // logical reading order from geometry, independent of whether the
            // producer stored the run visually or logically. Per-span glyph
            // order is corrected separately by `push_span_text_bidi`.
            ordered = Self::order_pure_rtl_spans(spans);
        }
        // Mixed RTL+Latin MCIDs keep raw order (full UAX #9 bidi deferred).
        ordered
    }

    /// Order a pure-RTL MCID's spans into logical reading order: group spans
    /// into visual lines using a **font-relative** vertical tolerance, then
    /// emit each line right-to-left (X descending).
    ///
    /// A fixed quantized row band (`row_aware_span_cmp_rtl` with the global
    /// `ROW_BAND_TOLERANCE_PT`) over-segments Arabic lines. Producers routinely
    /// draw zero-advance glyphs — hamza seats, shadda/kasra marks, and even
    /// whole consonants positioned by a separate zero-width show — 1–3 pt off
    /// the baseline. A coarse fixed band rounds those into adjacent rows, which
    /// then emit before or after the body of the line and scatter the run (the
    /// telltale leading run of stray alef/hamza glyphs). Banding by a tolerance
    /// proportional to the glyph size keeps one jittery line intact while still
    /// separating genuinely distinct lines, whose leading is ~1.2× the font
    /// size — comfortably beyond the tolerance. Per-span glyph order is fixed
    /// separately by [`push_span_text_bidi`]; this function only fixes the order
    /// in which spans are emitted.
    fn order_pure_rtl_spans(spans: &[crate::layout::TextSpan]) -> Vec<&crate::layout::TextSpan> {
        use crate::utils::safe_float_cmp;
        let mut by_y: Vec<&crate::layout::TextSpan> = spans.iter().collect();
        // Stable sort, Y descending (top of page first). Ties keep extraction
        // (content-stream) order; the X-descending pass below refines each line.
        by_y.sort_by(|a, b| safe_float_cmp(b.bbox.y, a.bbox.y));

        let mut out: Vec<&crate::layout::TextSpan> = Vec::with_capacity(spans.len());
        let mut line: Vec<&crate::layout::TextSpan> = Vec::new();
        let mut anchor_y = f32::NAN;
        let mut tol = 0.0f32;
        for s in by_y {
            let fs = if s.font_size.is_finite() && s.font_size > 1.0 {
                s.font_size
            } else {
                10.0
            };
            let starts_new_line =
                anchor_y.is_finite() && (!s.bbox.y.is_finite() || anchor_y - s.bbox.y > tol);
            if anchor_y.is_nan() || starts_new_line {
                if !line.is_empty() {
                    line.sort_by(|a, b| safe_float_cmp(b.bbox.x, a.bbox.x));
                    out.append(&mut line);
                }
                anchor_y = s.bbox.y;
                tol = 0.5 * fs;
            }
            line.push(s);
        }
        if !line.is_empty() {
            line.sort_by(|a, b| safe_float_cmp(b.bbox.x, a.bbox.x));
            out.append(&mut line);
        }
        out
    }

    /// Pre-pass for the cross-span Arabic GLYPH-interleave defect. Some producers
    /// draw one Arabic word as a multi-glyph body span PLUS separate zero-width
    /// mark / consonant spans positioned by their own show, each at its true x.
    /// The atom-level span sort ([`order_pure_rtl_spans`]) then orders those spans
    /// as whole units and reverses each independently, so a zero-width glyph whose
    /// x falls *inside* a sibling span's x-extent lands at a word edge instead of
    /// interleaved — `الثدييات` extracts as `ثالدييات`.
    ///
    /// Returns `Some(owned spans)` with each affected visual LINE collapsed into a
    /// single visual-order span (so the downstream [`push_span_text_bidi`] reverse
    /// produces correct logical order), or `None` when no line exhibits the defect
    /// (then the caller uses the original spans, byte-identical). Gated tightly:
    /// fires only on a pure-RTL line (no ASCII-alpha) that actually contains a
    /// zero-width span interleaved inside another — a page with no such interleave
    /// (BidiSample, ArabicCIDTrueType-logical, hebrew_mirrored) returns `None`.
    fn merge_interleaved_rtl_lines(
        spans: &[crate::layout::TextSpan],
    ) -> Option<Vec<crate::layout::TextSpan>> {
        use crate::utils::safe_float_cmp;
        if spans.len() < 3 {
            return None;
        }
        // Group into visual lines by a font-relative Y tolerance (mirrors
        // order_pure_rtl_spans banding).
        let mut by_y: Vec<&crate::layout::TextSpan> = spans.iter().collect();
        by_y.sort_by(|a, b| safe_float_cmp(b.bbox.y, a.bbox.y));
        let mut lines: Vec<Vec<&crate::layout::TextSpan>> = Vec::new();
        // Roll the line-break reference forward to the PREVIOUS span's y rather
        // than pinning it to the band's first (topmost) span. RTL producers seat
        // a line's glyphs across a few points of vertical jitter — combining
        // marks ride high, a line-final letter can sit a few points low (P2: a
        // width-0 final glyph at dy≈3pt below the baseline). Against a fixed top
        // anchor the line's own span furthest above the baseline sets the band
        // ceiling, so the lowest glyph can exceed `0.5·fs` and split onto its own
        // "line" — which then reverses and lands after the sentence terminator,
        // detaching the line's final letter. Comparing each span to its immediate
        // predecessor keeps a line whose internal step is < tol intact while a
        // real inter-line gap (leading ≈ one full em, well over tol) still opens
        // the next band.
        let mut prev_y = f32::NAN;
        let mut tol = 0.0f32;
        for s in by_y {
            let fs = if s.font_size.is_finite() && s.font_size > 1.0 {
                s.font_size
            } else {
                10.0
            };
            let new_line = prev_y.is_finite() && (!s.bbox.y.is_finite() || prev_y - s.bbox.y > tol);
            if prev_y.is_nan() || new_line {
                lines.push(Vec::new());
                tol = 0.5 * fs;
            }
            prev_y = s.bbox.y;
            lines.last_mut().unwrap().push(s);
        }

        let mut any_gated = false;
        let mut out: Vec<crate::layout::TextSpan> = Vec::with_capacity(spans.len());
        for line in &lines {
            if Self::rtl_line_needs_glyph_reorder(line) {
                any_gated = true;
                out.push(Self::merge_rtl_line_to_visual_span(line));
            } else {
                out.extend(line.iter().map(|s| (*s).clone()));
            }
        }
        if any_gated {
            Some(out)
        } else {
            None
        }
    }

    /// True when a visual line exhibits the zero-width-glyph interleave defect:
    /// (1) pure-RTL — no span carries an ASCII-alphabetic char and at least one
    /// carries an RTL letter; AND (2) a zero-width span's x lies STRICTLY inside
    /// another span's `[x, x+width]` on the line. Both are required so ordinary
    /// pure-RTL text (no interleave) and any mixed-Latin line are left untouched.
    fn rtl_line_needs_glyph_reorder(line: &[&crate::layout::TextSpan]) -> bool {
        use crate::text::rtl_detector::is_rtl_text;
        if line.len() < 2 {
            return false;
        }
        let mut has_rtl = false;
        for s in line {
            for c in s.text.chars() {
                if c.is_ascii_alphabetic() {
                    return false; // mixed Latin — not our case
                }
                if is_rtl_text(c as u32) {
                    has_rtl = true;
                }
            }
        }
        if !has_rtl {
            return false;
        }
        line.iter().any(|m| {
            m.bbox.width.abs() < 0.01
                && line.iter().any(|b| {
                    !std::ptr::eq(*m, *b)
                        && b.bbox.width > 0.01
                        && m.bbox.x > b.bbox.x
                        && m.bbox.x < b.bbox.x + b.bbox.width
                })
        })
    }

    /// Collapse a gated RTL visual line into one VISUAL-order span: explode every
    /// span into per-glyph `(x, char)` (reusing the `to_chars` advance arithmetic),
    /// drop producer shatter spaces, sort base letters by ascending x (visual
    /// left-to-right), bind each combining mark to its nearest base, and re-insert
    /// a single space at genuine inter-word x-gaps. The downstream
    /// [`push_span_text_bidi`] then reverses this to correct logical order with
    /// marks kept attached (`reverse_rtl_keeping_marks`).
    /// Private-use sentinel that [`merge_rtl_line_to_visual_span`] emits in place
    /// of a SPACE at an AUTHORITATIVE producer-segmented Arabic word boundary, so
    /// the downstream [`strip_interior_arabic_spaces`] (which strips only U+0020)
    /// leaves it intact instead of mistaking a genuine word break for a
    /// cursive-shatter artefact. Every output site restores it to a SPACE right
    /// after the strip ([`push_span_text_bidi`] for plain text,
    /// [`apply_rtl_logical_order_to_ordered_spans`] for md/html). U+F8FF is in the
    /// Unicode private-use area and never appears in real producer text reaching
    /// the pure-RTL merge path.
    const RTL_WORD_BOUNDARY: char = '\u{F8FF}';

    fn merge_rtl_line_to_visual_span(line: &[&crate::layout::TextSpan]) -> crate::layout::TextSpan {
        use crate::text::rtl_detector::is_rtl_diacritic;
        use crate::utils::safe_float_cmp;
        // Explode to glyphs: split bases from combining marks, DROP shatter spaces
        // that are interior to a multi-glyph span, but record the x of each
        // STANDALONE space span — those are the producer's real word boundaries
        // (geometric gap-thresholding is unreliable for cursive Arabic, so we use
        // the producer's own segmentation instead).
        let mut bases: Vec<(f32, char)> = Vec::new();
        let mut marks: Vec<(f32, char)> = Vec::new();
        let mut word_space_x: Vec<f32> = Vec::new();
        for s in line {
            if !s.text.is_empty() && s.text.chars().all(|c| c.is_whitespace()) {
                word_space_x.push(s.bbox.x + s.bbox.width * 0.5);
                continue;
            }
            // Pre-collect the span's chars so a whitespace glyph can see its
            // non-mark neighbours (to tell a cursive-join shatter space from a
            // genuine word break).
            let span_chars: Vec<char> = s.to_chars().into_iter().map(|t| t.char).collect();
            for (idx, tc) in s.to_chars().into_iter().enumerate() {
                let c = tc.char;
                if c.is_whitespace() {
                    // ISO 32000-1 §14.8.2.3.3: a SPACE that borders a
                    // NON-CURSIVE token (clause punctuation / symbol — not an
                    // Arabic/Hebrew letter and not a digit) is a real word
                    // break, so record its x. A space flanked by cursive letters
                    // is the producer's intra-word shatter (dropped), and a
                    // space between digits is a thousands separator (dropped) —
                    // neither is a word boundary.
                    use crate::text::rtl_detector::{
                        is_arabic_letter, is_arabic_number, is_hebrew_letter,
                    };
                    let neighbour = |it: &mut dyn Iterator<Item = &char>| -> Option<char> {
                        it.copied()
                            .find(|&p| !p.is_whitespace() && !is_rtl_diacritic(p as u32))
                    };
                    let is_boundary_marker = |o: Option<char>| {
                        o.is_some_and(|p| {
                            let u = p as u32;
                            !is_arabic_letter(u)
                                && !is_hebrew_letter(u)
                                && !is_arabic_number(u)
                                && !p.is_ascii_digit()
                        })
                    };
                    let prev = neighbour(&mut span_chars[..idx].iter().rev());
                    let next = neighbour(&mut span_chars[idx + 1..].iter());
                    if is_boundary_marker(prev) || is_boundary_marker(next) {
                        word_space_x.push(tc.bbox.x + tc.bbox.width * 0.5);
                    }
                    continue; // not emitted as a glyph either way
                }
                if is_rtl_diacritic(c as u32) {
                    marks.push((tc.bbox.x, c));
                } else {
                    bases.push((tc.bbox.x, c));
                }
            }
        }
        if bases.is_empty() {
            return (*line[0]).clone();
        }
        bases.sort_by(|a, b| safe_float_cmp(a.0, b.0));
        // Attach each mark to the nearest base by x; build base→trailing-marks.
        let mut trailing: Vec<Vec<char>> = vec![Vec::new(); bases.len()];
        for (mx, mc) in &marks {
            let mut best = 0usize;
            let mut best_d = f32::MAX;
            for (i, (bx, _)) in bases.iter().enumerate() {
                let d = (bx - mx).abs();
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            trailing[best].push(*mc);
        }
        // Emit visual (ascending-x) order: each base then its marks, with a single
        // word-boundary marker wherever a producer word-boundary x falls between two
        // bases. The marker is the private-use sentinel [`Self::RTL_WORD_BOUNDARY`],
        // not a plain SPACE, so the downstream `strip_interior_arabic_spaces` (which
        // only removes U+0020) cannot mistake this AUTHORITATIVE producer-segmented
        // word break for a cursive-shatter artefact and delete it; each output site
        // restores it to a SPACE right after the strip. The downstream reverse maps
        // this to logical order with words intact.
        let mut text = String::new();
        let mut prev_x: Option<f32> = None;
        for (i, (bx, bc)) in bases.iter().enumerate() {
            if let Some(px) = prev_x {
                if word_space_x.iter().any(|sx| *sx > px && *sx < *bx)
                    && !text.ends_with(Self::RTL_WORD_BOUNDARY)
                {
                    text.push(Self::RTL_WORD_BOUNDARY);
                }
            }
            text.push(*bc);
            for m in &trailing[i] {
                text.push(*m);
            }
            prev_x = Some(*bx);
        }
        // Build the merged span from the line's first span, spanning the line.
        let mut merged = (*line[0]).clone();
        let x_min = line.iter().map(|s| s.bbox.x).fold(f32::MAX, f32::min);
        let x_max = line
            .iter()
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::MIN, f32::max);
        merged.text = text;
        merged.bbox.x = x_min;
        merged.bbox.width = (x_max - x_min).max(0.0);
        merged.char_widths = Vec::new();
        merged
    }

    /// Port of the plain-text RTL reading-order correction to the converter
    /// pipeline's [`crate::pipeline::OrderedTextSpan`] sequence, so `to_markdown`
    /// and `to_html` produce logical-order Arabic/Hebrew.
    ///
    /// The plain-text path fixes RTL during structure assembly
    /// ([`order_pure_rtl_spans`] for span order, [`push_span_text_bidi`] for
    /// per-glyph order). The converter pipeline never reaches those — it orders
    /// spans by MCID/geometry and the converters emit each span's text verbatim
    /// — so visual-order Arabic leaked through reversed and scrambled. This pass
    /// groups the reading-order sequence into visual lines by a font-relative Y
    /// tolerance (matching the converters' own line-break test) and, for each
    /// line that is purely right-to-left, emits the spans rightmost-first and
    /// rewrites each span's text to logical order:
    /// - a pure-RTL span: strip interior cursive-join spaces (§14.8.2.3.3) then
    ///   reverse keeping combining marks attached ([`reverse_rtl_keeping_marks`]);
    /// - a neutral-only span: reverse so trailing punctuation re-attaches to the
    ///   preceding word (UAX #9 N1/N2, [`is_reversible_rtl_neutral_span`]).
    ///
    /// Mixed RTL+Latin lines are left untouched (full UAX #9 deferred), and the
    /// whole pass is skipped when the page has no RTL characters.
    ///
    /// KNOWN LIMITATION (md/html only): this pass orders a pure-RTL line by
    /// sorting whole SPANS by `bbox.x` (rightmost first), so it cannot place a
    /// standalone zero-width glyph (e.g. an Arabic qaf whose x falls inside an
    /// already-merged word span) at its correct *intra-word* slot — the glyph
    /// sorts before/after the word instead (`القهوة` → `قالهوة`). The plain-text
    /// path avoids this by reconstructing at the GLYPH level
    /// ([`merge_interleaved_rtl_lines`] / [`merge_rtl_line_to_visual_span`],
    /// which explode each span to per-glyph bases via `to_chars`). To make the
    /// md/html path match the text path, either run `merge_interleaved_rtl_lines`
    /// on the raw spans before the pipeline orders them, or rebuild each gated
    /// line here at the glyph level instead of the whole-span x-sort.
    fn apply_rtl_logical_order_to_ordered_spans(spans: &mut [crate::pipeline::OrderedTextSpan]) {
        use crate::text::rtl_detector::is_rtl_text;
        use crate::utils::safe_float_cmp;

        let has_any_rtl = |s: &crate::pipeline::OrderedTextSpan| {
            s.span.text.chars().any(|c| is_rtl_text(c as u32))
        };
        if !spans.iter().any(has_any_rtl) {
            return; // fast path: pure-LTR page is byte-identical
        }

        // The converter pipeline's reading order can interleave the spans of a
        // single RTL line (the MCID/XYCut order is not strictly top-to-bottom),
        // which would break the consecutive line-grouping below — a display
        // heading then scatters into one `#` per fragment. On a page that is
        // dominantly right-to-left, re-sort by Y first so each visual line is
        // contiguous; Y-descending IS the reading order there. Mixed / LTR-
        // dominant pages keep the pipeline order so LTR runs are not disturbed.
        let rtl_letters = spans
            .iter()
            .flat_map(|s| s.span.text.chars())
            .filter(|c| is_rtl_text(*c as u32))
            .count();
        let latin_letters = spans
            .iter()
            .flat_map(|s| s.span.text.chars())
            .filter(|c| c.is_ascii_alphabetic())
            .count();
        if rtl_letters > latin_letters.max(1) * 2 {
            spans.sort_by(|a, b| safe_float_cmp(b.span.bbox.y, a.span.bbox.y));
        }

        let n = spans.len();
        let mut i = 0;
        while i < n {
            // Group the maximal run of spans on the same visual line, using the
            // same font-relative tolerance the converters use for line breaks.
            let anchor_y = spans[i].span.bbox.y;
            let mut j = i + 1;
            while j < n {
                let fs = spans[i]
                    .span
                    .font_size
                    .max(spans[j].span.font_size)
                    .max(1.0);
                if !spans[j].span.bbox.y.is_finite()
                    || (spans[j].span.bbox.y - anchor_y).abs() > 0.5 * fs
                {
                    break;
                }
                j += 1;
            }
            let line = &mut spans[i..j];

            let has_rtl = line.iter().any(has_any_rtl);
            let has_latin = line
                .iter()
                .any(|s| s.span.text.chars().any(|c| c.is_ascii_alphabetic()));
            if has_rtl && !has_latin {
                // Logical RTL order is rightmost glyph first.
                line.sort_by(|a, b| safe_float_cmp(b.span.bbox.x, a.span.bbox.x));
                // Snap every span on this line to a single baseline. RTL
                // producers jitter glyphs a few points off the baseline (hamza
                // seats, marks), and the converters break a line whenever the
                // Y delta between consecutive spans exceeds ~0.5em — which,
                // after the X-descending reorder, would shatter one line into
                // many spurious one-word "lines" (each then mis-promoted to a
                // heading). Collapsing the jitter keeps the line intact.
                for s in line.iter_mut() {
                    s.span.bbox.y = anchor_y;
                }
                for s in line.iter_mut() {
                    let mut rtl = 0usize;
                    let mut latin = false;
                    for c in s.span.text.chars() {
                        if c.is_whitespace() {
                            continue;
                        }
                        if c.is_ascii_alphabetic() {
                            latin = true;
                            break;
                        }
                        if is_rtl_text(c as u32) {
                            rtl += 1;
                        }
                    }
                    if rtl >= 2 && !latin {
                        s.span.text = Self::reverse_rtl_keeping_marks(
                            &Self::strip_interior_arabic_spaces(&s.span.text),
                        )
                        .replace(Self::RTL_WORD_BOUNDARY, " ");
                    } else if Self::is_reversible_rtl_neutral_span(&s.span.text) {
                        s.span.text = s.span.text.chars().rev().collect();
                    }
                }
            }
            i = j;
        }

        // The converters re-sort by `reading_order` before emitting, so the
        // span-order changes above (the Y pre-sort and the per-line X-descending
        // reorder) only take effect once that field reflects the new sequence.
        for (idx, s) in spans.iter_mut().enumerate() {
            s.reading_order = idx;
            // Defensive: restore any word-boundary sentinel that the reorder
            // branch above did not reach (e.g. a merged pure-RTL span that ended
            // up Y-banded with a Latin neighbour so its line took the LTR path),
            // so the marker can never leak into md/html output.
            if s.span.text.contains(Self::RTL_WORD_BOUNDARY) {
                s.span.text = s.span.text.replace(Self::RTL_WORD_BOUNDARY, " ");
            }
        }
    }

    ///
    /// Used by paths that operate on raw spans rather than ordered
    /// spans (`extract_page_text`, `extract_structured`,
    /// `extract_spans_with_reading_order`). Mutates each covered span's
    /// text to the replacement (run-first only) or clears it
    /// (continuation / suppress-only / non-first-page coverage); fully
    /// suppressed spans are removed.
    ///
    /// Untagged documents and pages with no coverage are no-ops.
    pub(crate) fn apply_actualtext_to_spans(
        &self,
        page_index: usize,
        spans: &mut Vec<crate::layout::TextSpan>,
    ) {
        let Some(idx) = self.actualtext_index() else {
            return;
        };
        if idx.covered_mcids.is_empty() {
            return;
        }
        let mc_wins: HashSet<u32> = self
            .mc_actualtext_mcids
            .lock_or_recover()
            .get(&page_index)
            .cloned()
            .unwrap_or_default();

        let default_scope = crate::structure::McidScope::Page(page_index as u32);
        // Visibility = "has at least one raw span at this (scope, mcid)".
        // glyph_text accumulates each key's rendered text for the §14.9.4
        // conformance gate (decline destructive replacements).
        let mut present: HashSet<(crate::structure::McidScope, u32)> = HashSet::new();
        let mut glyph_text: HashMap<(crate::structure::McidScope, u32), String> = HashMap::new();
        for s in spans.iter() {
            if let Some(m) = s.mcid {
                let scope = s.mcid_scope.clone().unwrap_or(default_scope.clone());
                present.insert((scope.clone(), m));
                glyph_text.entry((scope, m)).or_default().push_str(&s.text);
            }
        }
        // Walk the structure-tree's per-page MCID order so the
        // consecutive-run dedup matches the assemblers'.
        let mcid_order = self
            .struct_tree_marked()
            .map(|t| self.cached_mcid_order_for_page(&t, page_index as u32))
            .unwrap_or_default();
        let actions = Self::actualtext_actions_for_page(
            Some(&idx),
            &mcid_order,
            |scope, m| present.contains(&(scope.clone(), m)),
            &mc_wins,
            &glyph_text,
        );
        if actions.is_empty() {
            return;
        }

        // Apply actions to the raw spans. EmitAndSuppress mutates the
        // first span of the (scope, mcid) key; subsequent spans for
        // the same key are dropped (so a key with multiple spans
        // collapses to one span carrying the replacement). Suppress
        // drops every span with that key.
        let mut emit_used: HashSet<(crate::structure::McidScope, u32)> = HashSet::new();
        let mut drop_idx: Vec<usize> = Vec::new();
        for (i, s) in spans.iter_mut().enumerate() {
            let Some(m) = s.mcid else { continue };
            let scope = s.mcid_scope.clone().unwrap_or(default_scope.clone());
            let key = (scope, m);
            match actions.get(&key) {
                Some(ActualTextAction::EmitAndSuppress(repl)) => {
                    if emit_used.insert(key) {
                        s.text = repl.to_string();
                    } else {
                        s.text.clear();
                        drop_idx.push(i);
                    }
                }
                Some(ActualTextAction::Suppress) => {
                    s.text.clear();
                    drop_idx.push(i);
                }
                None => {}
            }
        }
        for &i in drop_idx.iter().rev() {
            spans.remove(i);
        }
    }

    /// Apply struct-tree-scope `/ActualText` to a vector of ordered
    /// spans, in place. Mirrors [`Self::apply_actualtext_to_spans`]
    /// over the converters' [`crate::pipeline::OrderedTextSpan`]
    /// shape; renumbers `reading_order` after dropping suppressed
    /// spans so downstream converters see a contiguous sequence.
    /// Tag ordered spans that fall within a `/Link` annotation's rectangle
    /// with its resolved URI, so the markdown/HTML converters can emit
    /// hyperlinks (ISO 32000-1 §12.5.6.5 Link annotations + §12.6.4.7 URI
    /// actions). Spans and link rectangles share PDF user-space coordinates,
    /// so a span is linked when its bbox centre lies inside the rectangle.
    pub(crate) fn apply_link_annotations_to_ordered_spans(
        &self,
        page_index: usize,
        ordered: &mut [crate::pipeline::OrderedTextSpan],
    ) {
        use crate::annotation_types::AnnotationSubtype;
        use crate::annotations::LinkAction;

        let annots = match self.get_annotations(page_index) {
            Ok(a) => a,
            Err(_) => return,
        };
        let mut links: Vec<([f32; 4], std::sync::Arc<str>)> = Vec::new();
        for a in &annots {
            if a.subtype_enum != AnnotationSubtype::Link {
                continue;
            }
            let uri = match &a.action {
                Some(LinkAction::Uri(u)) if !u.is_empty() => {
                    std::sync::Arc::<str>::from(u.as_str())
                }
                _ => continue,
            };
            if let Some(r) = a.rect {
                links.push((
                    [
                        r[0].min(r[2]) as f32,
                        r[1].min(r[3]) as f32,
                        r[0].max(r[2]) as f32,
                        r[1].max(r[3]) as f32,
                    ],
                    uri,
                ));
            }
        }
        if links.is_empty() {
            return;
        }
        for s in ordered.iter_mut() {
            let b = &s.span.bbox;
            let (sx0, sy0, sx1, sy1) = (b.x, b.y, b.x + b.width, b.y + b.height);
            // A span is linked when its bbox overlaps the annotation rectangle.
            // Overlap (rather than centre-in-rect) keeps the link when adjacent
            // runs are merged into one wide span the small link rect only
            // partially covers — the URL is preserved rather than lost.
            if let Some((_, uri)) = links
                .iter()
                .find(|(r, _)| sx0 < r[2] && sx1 > r[0] && sy0 < r[3] && sy1 > r[1])
            {
                s.link_uri = Some(uri.clone());
            }
        }
    }

    pub(crate) fn apply_actualtext_to_ordered_spans(
        &self,
        page_index: usize,
        ordered: &mut Vec<crate::pipeline::OrderedTextSpan>,
    ) {
        let Some(idx) = self.actualtext_index() else {
            return;
        };
        if idx.covered_mcids.is_empty() {
            return;
        }
        let mc_wins: HashSet<u32> = self
            .mc_actualtext_mcids
            .lock_or_recover()
            .get(&page_index)
            .cloned()
            .unwrap_or_default();

        let default_scope = crate::structure::McidScope::Page(page_index as u32);
        let mut present: HashSet<(crate::structure::McidScope, u32)> = HashSet::new();
        let mut glyph_text: HashMap<(crate::structure::McidScope, u32), String> = HashMap::new();
        for o in ordered.iter() {
            if let Some(m) = o.span.mcid {
                let scope = o.span.mcid_scope.clone().unwrap_or(default_scope.clone());
                present.insert((scope.clone(), m));
                glyph_text
                    .entry((scope, m))
                    .or_default()
                    .push_str(&o.span.text);
            }
        }
        let mcid_order = self
            .struct_tree_marked()
            .map(|t| self.cached_mcid_order_for_page(&t, page_index as u32))
            .unwrap_or_default();
        let actions = Self::actualtext_actions_for_page(
            Some(&idx),
            &mcid_order,
            |scope, m| present.contains(&(scope.clone(), m)),
            &mc_wins,
            &glyph_text,
        );
        if actions.is_empty() {
            return;
        }

        let mut emit_used: HashSet<(crate::structure::McidScope, u32)> = HashSet::new();
        for o in ordered.iter_mut() {
            let Some(m) = o.span.mcid else { continue };
            let scope = o.span.mcid_scope.clone().unwrap_or(default_scope.clone());
            let key = (scope, m);
            match actions.get(&key) {
                Some(ActualTextAction::EmitAndSuppress(repl)) => {
                    if emit_used.insert(key) {
                        o.span.text = repl.to_string();
                        o.actualtext_replacement = Some(repl.clone());
                    } else {
                        o.span.text.clear();
                        o.actualtext_replacement = Some(std::sync::Arc::from(""));
                    }
                }
                Some(ActualTextAction::Suppress) => {
                    o.span.text.clear();
                    o.actualtext_replacement = Some(std::sync::Arc::from(""));
                }
                None => {}
            }
        }

        ordered.retain(|o| !o.is_suppressed());
        for (i, o) in ordered.iter_mut().enumerate() {
            o.reading_order = i;
        }
    }

    /// Compute the per-page `MCID → ActualTextAction` map.
    ///
    /// Walks `mcid_order` (the structure-tree's per-page MCID sequence
    /// in pre-order) and groups consecutive covered MCIDs by the
    /// replacement text they share. Each group emits ONE replacement at
    /// the first visible-and-not-MC-scope-wins MCID; the rest of the
    /// group is marked `Suppress` (raw glyphs dropped). MCIDs whose
    /// `(page, mcid)` lands in `suppress_only` are always `Suppress`
    /// (their replacement already fired on a different page).
    ///
    /// `visible(mcid)` returns `true` when at least one span carries
    /// the MCID and survives all upstream filters (artifact / OCG /
    /// region). A run with zero visible MCIDs is dropped entirely (no
    /// emission, no suppression — nothing to drop).
    ///
    /// MCIDs in `mc_wins` keep the in-stream MC-scope `/ActualText`
    /// replacement applied by the extractor and are exempt from the
    /// ancestor struct-tree scope; they do not break the run dedup —
    /// the run can still find a non-MC-wins MCID to emit at.
    /// §14.9.4 conformance test for a struct-tree `/ActualText` replacement.
    ///
    /// Per ISO 32000-1 §14.9.4 (pdf.md:39253) an `/ActualText` value "shall be
    /// used as a replacement … providing text that is *equivalent to what a
    /// person would see when viewing the content*"; per §14.8.2.4 NOTE 2
    /// (pdf.md:37380) a conforming reader *may choose* whether to use it. We
    /// decline a replacement that is **destructive**: it would suppress glyphs
    /// carrying alphanumeric (letter/digit, any script) content while itself
    /// carrying none — e.g. a producer tagging whole words with `" "` or `"-"`.
    /// Such a value is not "equivalent to what a person would see", so we keep
    /// the rendered glyphs (extracted via ToUnicode, §14.8.2.4) instead.
    /// Legitimate ActualText — the spec's hyphenation EXAMPLE `(c)`→`k-`,
    /// ligature/soft-hyphen substitution (NOTE 3), any real-character
    /// replacement — is alphanumeric and passes.
    fn actual_text_is_destructive(replacement: &str, covered_glyphs: &str) -> bool {
        covered_glyphs.chars().any(char::is_alphanumeric)
            && !replacement.chars().any(char::is_alphanumeric)
    }

    fn actualtext_actions_for_page<F: Fn(&crate::structure::McidScope, u32) -> bool>(
        idx: Option<&crate::structure::ActualTextIndex>,
        mcid_order: &[(crate::structure::McidScope, u32)],
        visible: F,
        mc_wins: &HashSet<u32>,
        glyph_text: &HashMap<(crate::structure::McidScope, u32), String>,
    ) -> HashMap<(crate::structure::McidScope, u32), ActualTextAction> {
        let mut out: HashMap<(crate::structure::McidScope, u32), ActualTextAction> = HashMap::new();
        let Some(idx) = idx else {
            return out;
        };
        if idx.covered_mcids.is_empty() {
            return out;
        }

        // Two-pass walk to support runs that span the input order
        // perfectly: collect (scope, mcid, replacement?) tuples for
        // covered MCIDs on this page (across all scopes that render on
        // it), then group consecutive equal-replacement entries into
        // runs.
        //
        // Replacement = None for `suppress_only` entries and for
        // covered keys with no text (defensive — shouldn't happen
        // given the builder invariants).
        let mut entries: Vec<(crate::structure::McidScope, u32, Option<&str>)> = Vec::new();
        for (scope, m) in mcid_order {
            let key = (scope.clone(), *m);
            if !idx.covered_mcids.contains(&key) {
                continue;
            }
            if idx.suppress_only.contains(&key) {
                entries.push((scope.clone(), *m, None));
                continue;
            }
            let text = idx.mcid_to_actual_text.get(&key).map(|s| &**s);
            entries.push((scope.clone(), *m, text));
        }

        // Walk entries and assign actions per consecutive same-
        // replacement run.
        let mut i = 0usize;
        while i < entries.len() {
            let repl_opt = entries[i].2;
            // Find the end of the consecutive run sharing this
            // replacement (None matches None — i.e. suppress-only runs
            // also collapse).
            let mut j = i;
            while j < entries.len() && entries[j].2 == repl_opt {
                j += 1;
            }

            if let Some(repl) = repl_opt {
                // §14.9.4 conformance gate (pdf.md:39253 + NOTE 2 pdf.md:37380):
                // if this replacement would suppress alphanumeric glyphs while
                // carrying none itself, it is not "equivalent to what a person
                // would see" — decline it (emit no action for the run) so the
                // rendered glyphs survive. See `actual_text_is_destructive`.
                let run_glyphs: String = entries[i..j]
                    .iter()
                    .filter_map(|e| glyph_text.get(&(e.0.clone(), e.1)))
                    .map(String::as_str)
                    .collect();
                if Self::actual_text_is_destructive(repl, &run_glyphs) {
                    i = j;
                    continue;
                }
                // Find first emit-eligible entry (visible, not MC-wins).
                // MC-wins keys are skipped because their replacement
                // came from the extractor's in-stream BDC /ActualText.
                let mut emit_pick: Option<(crate::structure::McidScope, u32)> = None;
                for entry in &entries[i..j] {
                    if visible(&entry.0, entry.1) && !mc_wins.contains(&entry.1) {
                        emit_pick = Some((entry.0.clone(), entry.1));
                        break;
                    }
                }
                let repl_arc: std::sync::Arc<str> = std::sync::Arc::from(repl);
                for entry in &entries[i..j] {
                    if mc_wins.contains(&entry.1) {
                        // MC-scope wins: do not touch this MCID at all.
                        // The extractor's inline replacement reaches
                        // output unmodified.
                        continue;
                    }
                    let key = (entry.0.clone(), entry.1);
                    if emit_pick.as_ref() == Some(&key) {
                        out.insert(key, ActualTextAction::EmitAndSuppress(repl_arc.clone()));
                    } else {
                        out.insert(key, ActualTextAction::Suppress);
                    }
                }
            } else {
                // suppress_only run: every key is suppressed (no
                // emission). MC-wins MCIDs stay untouched.
                for entry in &entries[i..j] {
                    if mc_wins.contains(&entry.1) {
                        continue;
                    }
                    out.insert((entry.0.clone(), entry.1), ActualTextAction::Suppress);
                }
            }

            i = j;
        }
        out
    }

    /// Page's MCID reading order from the all-pages traversal cache
    /// (`structure_content_cache`, populated once). `build_context` previously
    /// re-walked the whole tree per page (≈ O(pages²) on a tagged document);
    /// the cached all-pages walk (#608) yields the same per-page order.
    pub(crate) fn cached_mcid_order_for_page(
        &self,
        struct_tree: &crate::structure::StructTreeRoot,
        page_index: u32,
    ) -> Vec<(crate::structure::McidScope, u32)> {
        if self.structure_content_cache.lock_or_recover().is_none() {
            let all_content = crate::structure::traverse_structure_tree_all_pages(struct_tree);
            *self.structure_content_cache.lock_or_recover() = Some(all_content);
        }
        self.structure_content_cache
            .lock_or_recover()
            .as_ref()
            .and_then(|c| c.get(&page_index))
            .map(|content| {
                content
                    .iter()
                    .filter_map(|c| {
                        // Word break markers have mcid=None; skip.
                        let m = c.mcid?;
                        // Page-scoped MCIDs default to Page(c.page) when
                        // the parser didn't capture a scope. New parses
                        // always populate `mcid_scope`; the unwrap_or
                        // is for legacy traversals only.
                        let scope = c
                            .mcid_scope
                            .clone()
                            .unwrap_or(crate::structure::McidScope::Page(c.page));
                        Some((scope, m))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Extract text from a Tagged PDF page using pre-computed structure traversal cache.
    ///
    /// This is the optimized version of `extract_text_structure_order` that uses
    /// the pre-built `structure_content_cache` for O(1) page content lookup instead
    /// of re-traversing the entire structure tree for each page.
    fn extract_text_structure_order_cached_with_spans(
        &self,
        page_index: usize,
        all_spans: Vec<TextSpan>,
        include_artifacts: bool,
    ) -> Result<String> {
        log::debug!(
            "Extracting text using cached structure order for page {}",
            page_index
        );

        if all_spans.is_empty() {
            let mut text = String::new();
            self.append_non_widget_annotation_text(page_index, &mut text);
            return Ok(text);
        }

        // Drop content marked /Artifact (PDF Spec ISO 32000-1:2008
        // §14.8.2.2 — headers, footers, page numbers, decorations) —
        // unless the caller opted in via `include_artifacts` (default
        // true). The geometric branch in `assemble_text_from_spans`
        // applies the same filter; tagged PDFs taking the structure-order
        // path must honour it too, otherwise artifact spans (including
        // any MC-scope `/ActualText` replacements inside an `/Artifact`
        // BDC) leak into output. Untagged-PDF running-header
        // detection runs at document level and feeds the same flag.
        let all_spans: Vec<TextSpan> = if include_artifacts {
            all_spans
        } else {
            all_spans
                .into_iter()
                .filter(|s| s.artifact_type.is_none())
                .collect()
        };

        // Step 2: Build MCID → Vec<TextSpan> map
        let mut mcid_map: HashMap<u32, Vec<TextSpan>> = HashMap::new();
        let mut spans_without_mcid: Vec<TextSpan> = Vec::new();

        for span in all_spans {
            if let Some(mcid) = span.mcid {
                mcid_map.entry(mcid).or_default().push(span);
            } else {
                spans_without_mcid.push(span);
            }
        }

        // Step 3: Get pre-computed ordered content for this page (O(1) lookup)
        let ordered_content_owned: Vec<crate::structure::OrderedContent>;
        let ordered_content = {
            let cache = self.structure_content_cache.lock_or_recover();
            ordered_content_owned = cache
                .as_ref()
                .and_then(|c| c.get(&(page_index as u32)))
                .cloned()
                .unwrap_or_default();
            &ordered_content_owned as &[crate::structure::OrderedContent]
        };

        // Resolve struct-tree-scope `/ActualText` via the mcid-driven
        // action map (see `actualtext_actions_for_page`). The index is
        // built once per document (cached). For untagged documents the
        // map stays empty and the assembler behaves exactly as before.
        let at_index = self.actualtext_index();
        // MC-scope-wins precedence set: MCIDs whose BDC carried inline
        // `/ActualText` keep the in-stream replacement (most specific
        // declaration) and are exempt from ancestor struct-tree
        // emissions.
        let mc_wins: HashSet<u32> = self
            .mc_actualtext_mcids
            .lock_or_recover()
            .get(&page_index)
            .cloned()
            .unwrap_or_default();
        let default_scope = crate::structure::McidScope::Page(page_index as u32);
        let mcid_order: Vec<(crate::structure::McidScope, u32)> = ordered_content
            .iter()
            .filter_map(|c| {
                c.mcid
                    .map(|m| (c.mcid_scope.clone().unwrap_or(default_scope.clone()), m))
            })
            .collect();
        // Per-key rendered glyph text for the §14.9.4 conformance gate.
        let mut glyph_text: HashMap<(crate::structure::McidScope, u32), String> = HashMap::new();
        for (scope, m) in &mcid_order {
            if let Some(sp) = mcid_map.get(m) {
                let joined: String = sp.iter().map(|s| s.text.as_str()).collect();
                glyph_text
                    .entry((scope.clone(), *m))
                    .or_default()
                    .push_str(&joined);
            }
        }
        let actions = Self::actualtext_actions_for_page(
            at_index.as_deref(),
            &mcid_order,
            |_scope, m| mcid_map.contains_key(&m),
            &mc_wins,
            &glyph_text,
        );

        log::debug!(
            "Cached structure content: {} items for page {}, {} MCIDs with spans, {} ActualText actions on this page",
            ordered_content.len(),
            page_index,
            mcid_map.len(),
            actions.len()
        );

        // Step 4: Assemble text in structure order
        let mut text = String::with_capacity(mcid_map.len() * 50);
        let mut prev_span: Option<TextSpan> = None;
        let mut prev_in_table = false;
        let mut consumed_mcids: HashSet<u32> = HashSet::new();

        for content in ordered_content {
            if content.is_word_break {
                if !text.is_empty() && !text.ends_with(' ') && !text.ends_with('\n') {
                    text.push(' ');
                }
                continue;
            }

            let Some(mcid) = content.mcid else {
                continue;
            };
            // ISO 32000-1 §14.7: a marked-content sequence's MCID is unique
            // within its content stream and is referenced from the structure
            // hierarchy at most once. A malformed struct tree that re-references
            // the same MCID multiple times would otherwise emit that MCID's
            // glyphs once per reference. Emit each MCID once. (A destructive
            // /ActualText replacement — now declined by the §14.9.4 conformance
            // gate — can mask this by collapsing each consecutive run to a
            // single emit.)
            if !consumed_mcids.insert(mcid) {
                continue;
            }
            let mcid_scope_key = content.mcid_scope.clone().unwrap_or(default_scope.clone());

            match actions.get(&(mcid_scope_key, mcid)) {
                Some(ActualTextAction::EmitAndSuppress(repl)) => {
                    consumed_mcids.insert(mcid);
                    if !text.is_empty() && !text.ends_with(' ') && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(repl);
                    continue;
                }
                Some(ActualTextAction::Suppress) => {
                    consumed_mcids.insert(mcid);
                    continue;
                }
                None => {}
            }

            if let Some(spans) = mcid_map.get(&mcid) {
                consumed_mcids.insert(mcid);
                let rtl_run = Self::mcid_run_is_pure_rtl(spans);
                // Repair the cross-span Arabic glyph-interleave defect (zero-width
                // mark/consonant spans landing at word edges) before ordering.
                let merged_rtl = Self::merge_interleaved_rtl_lines(spans);
                let use_spans: &[crate::layout::TextSpan] = merged_rtl.as_deref().unwrap_or(spans);
                for span in Self::order_mcid_spans(use_spans) {
                    if let Some(prev) = &prev_span {
                        let y_diff = (prev.bbox.y - span.bbox.y).abs();
                        if y_diff > Self::same_line_threshold(prev, span) {
                            // Suppress the break when a Hangul eojeol wrapped
                            // mid-syllable (no inter-eojeol space at the wrap), so
                            // the word stays whole for word-segmentation scoring.
                            if !Self::hangul_midword_line_wrap(&text, prev, span) {
                                Self::push_line_breaks(
                                    &mut text,
                                    prev,
                                    span,
                                    y_diff,
                                    content.in_table && prev_in_table,
                                );
                            }
                        } else if Self::should_insert_space(prev, span)
                            || Self::stacked_cell_needs_space(prev, span)
                        {
                            text.push(' ');
                        }
                    }

                    Self::push_span_text_bidi(&mut text, span, rtl_run);
                    prev_span = Some(span.clone());
                    prev_in_table = content.in_table;
                }
            }
        }

        // Append spans with MCIDs not referenced by the structure tree
        let mut unconsumed: Vec<(&u32, &Vec<TextSpan>)> = mcid_map
            .iter()
            .filter(|(mcid, _)| !consumed_mcids.contains(mcid))
            .collect();
        unconsumed.sort_by_key(|(mcid, _)| **mcid);
        if !unconsumed.is_empty() {
            log::debug!(
                "Appending {} unreferenced MCIDs (e.g., from Form XObjects without StructParents)",
                unconsumed.len()
            );
            for (_mcid, spans) in &unconsumed {
                let rtl_run = Self::mcid_run_is_pure_rtl(spans);
                for span in *spans {
                    if let Some(prev) = &prev_span {
                        let y_diff = (prev.bbox.y - span.bbox.y).abs();
                        if y_diff > Self::same_line_threshold(prev, span) {
                            text.push('\n');
                        } else if Self::should_insert_space(prev, span) {
                            text.push(' ');
                        }
                    }
                    Self::push_span_text_bidi(&mut text, span, rtl_run);
                    prev_span = Some(span.clone());
                }
            }
        }

        // Append any spans without MCID (including widget/form field spans) sorted by position
        if !spans_without_mcid.is_empty() {
            log::debug!(
                "Found {} text spans without MCID (including form field widgets) - appending sorted by position",
                spans_without_mcid.len()
            );
            // Row-aware sort: Y-band descending (top→bottom), then X ascending.
            crate::utils::sort_by_row_band(&mut spans_without_mcid, |s| s.bbox.y, |s| s.bbox.x);
            for span in &spans_without_mcid {
                if let Some(prev) = &prev_span {
                    let y_diff = (prev.bbox.y - span.bbox.y).abs();
                    if y_diff > Self::same_line_threshold(prev, span) {
                        text.push('\n');
                    } else if Self::should_insert_space(prev, span) {
                        text.push(' ');
                    }
                }
                Self::push_span_text_bidi(&mut text, span, false);
                prev_span = Some(span.clone());
            }
        }

        // Annotation text is already included via annotation_content_spans() in
        // extract_spans() — do NOT call append_non_widget_annotation_text() here
        // (would cause double-emission of all annotation text).

        Ok(text)
    }

    /// Extract text spans from a page (PDF spec compliant - RECOMMENDED).
    ///
    /// This is the recommended method for text extraction. It extracts complete
    /// text strings as the PDF provides them via Tj/TJ operators, following the
    /// PDF specification ISO 32000-1:2008.
    ///
    /// # Benefits over extract_chars
    /// - Avoids overlapping character issues
    /// - Preserves PDF's text positioning intent
    /// - More robust for complex layouts
    /// - Matches industry best practices (PyMuPDF, etc.)
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Returns
    ///
    /// Vector of TextSpan objects in reading order
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::PdfDocument;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("document.pdf")?;
    /// let spans = doc.extract_spans(0)?;
    /// for span in spans {
    ///     println!("Text: {} at ({}, {})", span.text, span.bbox.x, span.bbox.y);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_spans(&self, page_index: usize) -> Result<Vec<crate::layout::TextSpan>> {
        // Serve repeat per-page extractions from cache (the converters reach
        // here twice per page; see `page_spans_cache`).
        if let Some(cached) = self.page_spans_cache.lock_or_recover().get(&page_index) {
            return Ok((**cached).clone());
        }
        let spans = self.extract_spans_raw(page_index)?;
        let spans = self.postprocess_spans(page_index, spans)?;
        self.page_spans_cache
            .lock_or_recover()
            .insert(page_index, std::sync::Arc::new(spans.clone()));
        Ok(spans)
    }

    fn extract_spans_filtered(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        let spans = self.extract_spans_raw_filtered(page_index, excluded_layers, excluded_inks)?;
        self.postprocess_spans(page_index, spans)
    }

    /// Map a span rectangle (already translated so the page origin is at
    /// `(0, 0)`) through a clockwise page `/Rotate` of `rot` degrees, returning
    /// the axis-aligned bounding box in the displayed coordinate frame.
    ///
    /// `page_w` / `page_h` are the unrotated page dimensions; for 90° / 270° the
    /// displayed page is `page_h × page_w`. Per ISO 32000-1:2008 §7.7.3.3 the
    /// rotation is clockwise and §8.3.3 gives the point transform. `rot` must be
    /// a normalised multiple of 90 (`0/90/180/270`); any other value returns the
    /// rectangle unchanged. `rot == 0` is the identity and `rot == 180` is
    /// numerically identical to the legacy mirror, preserving byte-for-byte
    /// output for unrotated and 180° pages.
    pub(crate) fn rotate_span_bbox(
        bbox: crate::geometry::Rect,
        rot: i32,
        page_w: f32,
        page_h: f32,
    ) -> crate::geometry::Rect {
        // Map a point (y-up) by the clockwise display rotation.
        let map = |x: f32, y: f32| -> (f32, f32) {
            match rot {
                90 => (y, page_w - x),
                180 => (page_w - x, page_h - y),
                270 => (page_h - y, x),
                _ => (x, y),
            }
        };
        let (ax, ay) = map(bbox.x, bbox.y);
        let (bx, by) = map(bbox.x + bbox.width, bbox.y + bbox.height);
        crate::geometry::Rect::new(ax.min(bx), ay.min(by), (ax - bx).abs(), (ay - by).abs())
    }

    /// Map a single span's bbox into the displayed frame for a `/Rotate`d page
    /// (translate to origin → [`rotate_span_bbox`] → translate back).
    fn map_span_into_rotated_frame(
        s: &mut crate::layout::TextSpan,
        rot: i32,
        llx: f32,
        lly: f32,
        w: f32,
        h: f32,
    ) {
        let rel =
            crate::geometry::Rect::new(s.bbox.x - llx, s.bbox.y - lly, s.bbox.width, s.bbox.height);
        let m = Self::rotate_span_bbox(rel, rot, w, h);
        s.bbox.x = llx + m.x;
        s.bbox.y = lly + m.y;
        s.bbox.width = m.width;
        s.bbox.height = m.height;
    }

    /// Order rotated runs that were segregated out of the horizontal reading
    /// flow. Spans drawn with a rotated text matrix (`rotation_degrees != 0`)
    /// break the axis-aligned assumptions of the row-band / XY-cut sort, so they
    /// are pulled out, ordered here, and appended as their own blocks. Runs are
    /// grouped by rotation (first-seen group order preserved); within a group
    /// each span's origin is rotated back into an upright frame and the standard
    /// row-aware comparator (top→bottom, left→right) is applied there.
    pub(crate) fn order_rotated_blocks(
        spans: Vec<crate::layout::TextSpan>,
    ) -> Vec<crate::layout::TextSpan> {
        let mut groups: Vec<(f32, Vec<crate::layout::TextSpan>)> = Vec::new();
        for s in spans {
            let key = s.rotation_degrees;
            match groups.iter_mut().find(|(k, _)| (*k - key).abs() < 0.5) {
                Some(g) => g.1.push(s),
                None => groups.push((key, vec![s])),
            }
        }
        let mut out = Vec::new();
        for (deg, mut group) in groups {
            let (sin, cos) = (-deg).to_radians().sin_cos();
            // Upright frame: rotate each origin by -deg, then read top→bottom,
            // left→right exactly as horizontal text.
            group.sort_by(|a, b| {
                let ax = a.bbox.x * cos - a.bbox.y * sin;
                let ay = a.bbox.x * sin + a.bbox.y * cos;
                let bx = b.bbox.x * cos - b.bbox.y * sin;
                let by = b.bbox.x * sin + b.bbox.y * cos;
                crate::utils::row_aware_span_cmp(ay, ax, by, bx)
            });
            out.extend(group);
        }
        out
    }

    /// Re-attach an oversized lone leading capital (a drop-cap / table-title
    /// initial that the producer set in a larger font, so it became its own
    /// span) to the body run immediately to its right on the same line —
    /// otherwise reading-order strands it (`TABLE` → `T` … `ABLE`).
    ///
    /// Conservative gates so prose drop-caps / standalone capitals aren't glued
    /// to the wrong word: the candidate must be a single uppercase ASCII letter
    /// at ≥1.5× the body run's font size, its right edge within ~0.3 em of the
    /// body's left edge, vertically overlapping it, and the body must start with
    /// a letter. Runs in raw span order before reading-order sorting.
    fn merge_drop_cap_initials(spans: &mut Vec<crate::layout::TextSpan>) {
        let n = spans.len();
        if n < 2 {
            return;
        }
        // A genuine drop cap is oversized relative to the page's *normal* body
        // text, not merely relative to its right-hand neighbor. Inline math such
        // as "A_st" pairs a normal-size capital with a shrunken subscript; gating
        // on the neighbor alone would treat that capital as oversized and glue
        // "A" + "st" into "Ast". Anchor the size gate to the median size of
        // multi-character spans (real words) so a body-size capital cannot
        // qualify.
        let mut body_sizes: Vec<f32> = spans
            .iter()
            .filter(|s| s.font_size > 0.0 && s.text.chars().nth(1).is_some())
            .map(|s| s.font_size)
            .collect();
        if body_sizes.is_empty() {
            return;
        }
        body_sizes.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        let body_size = body_sizes[body_sizes.len() / 2];

        // Span indices sorted by left edge, and the widest font on the page, so
        // each initial only probes spans whose left edge falls in its narrow
        // candidate x-window (was a full O(n) rescan per initial). A continuation
        // satisfies `gap in [-fs*0.5, fs*0.12]`, i.e. its left edge is within
        // [init_right - max_fs*0.5, init_right + max_fs*0.12]; using the page max
        // font widens the window conservatively, and the exact per-candidate gap
        // test below reproduces the original filter — so this is byte-identical.
        let order: Vec<usize> = {
            let mut o: Vec<usize> = (0..n).collect();
            o.sort_by(|&a, &b| crate::utils::safe_float_cmp(spans[a].bbox.x, spans[b].bbox.x));
            o
        };
        let max_fs = spans.iter().map(|s| s.font_size).fold(0.0_f32, f32::max);

        // For each initial candidate, the closest qualifying body span to its right.
        let mut target: Vec<Option<usize>> = vec![None; n];
        for i in 0..n {
            let init = &spans[i];
            if init.text.chars().count() != 1 || init.font_size <= 0.0 {
                continue;
            }
            if !init
                .text
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
            {
                continue;
            }
            if init.font_size < body_size * 1.5 {
                continue; // initial must be clearly oversized vs normal body text
            }
            let init_right = init.bbox.x + init.bbox.width;
            // Candidates: spans whose left edge is in the conservative window.
            // Collect their indices and visit in ASCENDING ORIGINAL ORDER so the
            // strict-`<` min keeps the same first-wins tie-break as the old scan.
            let lo_x = init_right - max_fs * 0.5;
            let hi_x = init_right + max_fs * 0.12;
            let lo = order.partition_point(|&k| spans[k].bbox.x < lo_x);
            let hi = order.partition_point(|&k| spans[k].bbox.x <= hi_x);
            let mut cands: Vec<usize> = order[lo..hi].to_vec();
            cands.sort_unstable();
            let mut best: Option<usize> = None;
            let mut best_gap = f32::MAX;
            for &j in &cands {
                let body = &spans[j];
                if j == i || body.font_size <= 0.0 {
                    continue;
                }
                if !body.text.chars().next().is_some_and(|c| c.is_alphabetic()) {
                    continue;
                }
                // Continuation shares the initial's baseline (same text line). A
                // tall oversized initial also vertically overlaps the line *above*
                // it, so a raw bbox-overlap test would let it reach up and steal a
                // word from the previous line (alice_old: the 16.8pt "A" of "A very
                // heavy weight" overlapping "Or if" → "OrAif"). Baseline proximity
                // (≈ bbox bottom) keeps the merge on the initial's own line.
                if (init.bbox.y - body.bbox.y).abs() > body.font_size * 0.5 {
                    continue;
                }
                // Body immediately to the right, essentially touching. A genuine
                // oversized initial is the first glyph of one word ("T" of
                // "TABLE", "P" of "PENALTY"), so its continuation begins within a
                // hair of the initial's advance — never across a word space. A
                // word-space gap (~0.25 em) would wrongly glue a standalone "A"
                // or "I" onto the next word ("A Perspective" → "APerspective"),
                // so the upper bound stays well below it.
                let gap = body.bbox.x - init_right;
                if gap < -body.font_size * 0.5 || gap > body.font_size * 0.12 {
                    continue;
                }
                if gap.abs() < best_gap {
                    best_gap = gap.abs();
                    best = Some(j);
                }
            }
            target[i] = best;
        }

        let mut taken = vec![false; n];
        let mut remove = vec![false; n];
        for i in 0..n {
            let Some(j) = target[i] else { continue };
            if taken[j] || remove[j] || remove[i] {
                continue; // a body receives at most one initial
            }
            taken[j] = true;
            remove[i] = true;
            let init_text = spans[i].text.clone();
            let init_left = spans[i].bbox.x;
            let body = &mut spans[j];
            body.text = format!("{init_text}{}", body.text);
            let right = body.bbox.x + body.bbox.width;
            body.bbox.x = init_left.min(body.bbox.x);
            body.bbox.width = right - body.bbox.x;
        }
        let mut k = 0;
        spans.retain(|_| {
            let keep = !remove[k];
            k += 1;
            keep
        });
    }

    /// True for Computer-Modern (`CM*`) or symbol font names, after stripping a
    /// `ABCDEF+` subset tag. Used to scope the `¬`→`.` decimal recovery.
    fn is_cm_or_symbol_font(font_name: &str) -> bool {
        let base = font_name.split('+').next_back().unwrap_or(font_name);
        let lower = base.to_ascii_lowercase();
        lower.starts_with("cm") || lower.contains("symbol")
    }

    /// Replace a `¬` (U+00AC) that a math subset drew from its `logicalnot`
    /// slot as a decimal point. Two shapes are recovered:
    ///
    ///   - `digit ¬ digit`         → `digit.digit` (e.g. `1¬00` → `1.00`)
    ///   - `digit ¬ <space> digit` → `digit.digit` (e.g. `1¬ 00` → `1.00`)
    ///
    /// The second form covers subsets that emit a single space between the
    /// decimal glyph and the fractional digits; the lone separating space is
    /// dropped so the number reads as one token. The leading digit must abut
    /// `¬` directly in both shapes, so a genuinely spaced negation (`5 ¬ 3`,
    /// `A ¬ B`) is left untouched. Every other `¬` is preserved.
    fn fix_digit_logicalnot_decimal(text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\u{00AC}' && i > 0 && chars[i - 1].is_ascii_digit() {
                // Unspaced: digit ¬ digit.
                if chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()) {
                    out.push('.');
                    i += 1;
                    continue;
                }
                // Spaced: digit ¬ <single space> digit — drop the lone space.
                if chars.get(i + 1) == Some(&' ')
                    && chars.get(i + 2).is_some_and(|n| n.is_ascii_digit())
                {
                    out.push('.');
                    i += 2; // skip the ¬ and the single separating space
                    continue;
                }
            }
            out.push(c);
            i += 1;
        }
        out
    }

    /// Drop spans whose bbox lies ENTIRELY outside the page's MediaBox.
    ///
    /// PDFs that reuse one big Form XObject across pages (ExpertPdf and similar
    /// tools - see issue B1 / nougat_005.pdf) rely on the content stream's `W n`
    /// clip rectangle to hide the off-page portion. The text extractor does not
    /// honour `W n` yet, so without this filter a page emits every page's worth of
    /// spans at distinct but out-of-bounds Y coordinates. Spans that even
    /// PARTIALLY overlap the MediaBox are kept, so legitimate bleed / trim-mark
    /// content is never dropped.
    ///
    /// `get_page_media_box` returns `(llx, lly, urx, ury)` - absolute corner
    /// coordinates per ISO 32000-1 s7.7.3.3, NOT `(x, y, width, height)`.
    fn drop_offpage_spans(&self, page_index: usize, spans: &mut Vec<crate::layout::TextSpan>) {
        if let Ok((llx, lly, urx, ury)) = self.get_page_media_box(page_index) {
            const EDGE_TOLERANCE_PT: f32 = 2.0;
            // Normalise corners: some producers write the MediaBox with swapped
            // corners (e.g. `[0 792 612 0]`, ury < lly). Taking min/max makes the
            // bounds correct either way - without this a swapped box inverts the
            // test below and drops the whole page's legitimate text.
            let left = llx.min(urx) - EDGE_TOLERANCE_PT;
            let right = llx.max(urx) + EDGE_TOLERANCE_PT;
            let bottom = lly.min(ury) - EDGE_TOLERANCE_PT;
            let top = lly.max(ury) + EDGE_TOLERANCE_PT;
            spans.retain(|span| {
                let sx1 = span.bbox.x;
                let sx2 = span.bbox.x + span.bbox.width;
                let sy1 = span.bbox.y;
                let sy2 = span.bbox.y + span.bbox.height;
                sx2 > left && sx1 < right && sy2 > bottom && sy1 < top
            });
        }
    }

    fn postprocess_spans(
        &self,
        page_index: usize,
        raw_spans: Vec<crate::layout::TextSpan>,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        let mut spans = raw_spans;

        self.drop_offpage_spans(page_index, &mut spans);

        // Recover decimal points mis-decoded as `¬` (U+00AC) in Computer-Modern
        // math subsets, where the `/Differences` names the decimal slot
        // `logicalnot`. Only a `¬` sitting *directly between two digits* (no
        // space) is rewritten — real logic/set `¬` is always spaced, so this
        // cannot corrupt it.
        for span in &mut spans {
            if Self::is_cm_or_symbol_font(&span.font_name) && span.text.contains('\u{00AC}') {
                span.text = Self::fix_digit_logicalnot_decimal(&span.text);
            }
        }

        // Re-attach oversized lone leading capitals to their word before the
        // reading-order sort can strand them (drop-cap / table-title initials).
        Self::merge_drop_cap_initials(&mut spans);

        // Apply page /Rotate to span geometry BEFORE reading-order sorting.
        //
        // A page with a /Rotate entry must be read in its DISPLAYED orientation
        // or the row-aware sort emits text in the wrong order (pdf.js issue14415
        // is a 180° English page that otherwise comes out word- and line-reversed).
        //
        // The transform is applied selectively, because a rotated page carries two
        // very different kinds of run, distinguished by each span's own
        // `rotation_degrees` (the content-stream text-matrix rotation):
        //
        // * **Horizontal content (`rotation_degrees == 0`) on a 90°/270° page** —
        //   e.g. a landscape table stored rotated (`/Rotate 90`, MediaBox already
        //   landscape). This text is horizontal in raw user space, so it reads and
        //   groups correctly THERE. Rotating its bbox by ±90° only rotates the
        //   RECTANGLE, but `TextSpan::to_chars` still lays glyphs horizontally with
        //   raw advance widths and cannot express a now-vertical run, so every raw
        //   row collapses onto one displayed band and perpendicular columns fuse
        //   into one 1000+ char token (#804). These are LEFT RAW — matching
        //   `extract_chars`, which also returns raw coordinates.
        //
        // * **Rotated content (`rotation_degrees == ±90`) on a 90°/270° page** —
        //   e.g. a chart axis, a sideways table, or a whole landscape page authored
        //   by drawing every glyph sideways in a portrait MediaBox with `/Rotate 90`
        //   to present it upright. Here the page /Rotate must be applied so it
        //   COMBINES with the content rotation (which `order_rotated_blocks` undoes
        //   for ordering) into the correct upright displayed frame; leaving it raw
        //   reads the page sideways. These ARE mapped.
        //
        // 180° maps everything (text stays horizontal; both axes just mirror —
        // numerically identical to the legacy mirror).
        //
        // Captured so the same transform is applied to annotation spans appended
        // later (their /Rect is in unrotated page space too). `None` for rot == 0
        // or unknown media box — those pages keep raw geometry.
        let page_rotation: Option<(i32, f32, f32, f32, f32)> =
            match self.get_page_media_box(page_index) {
                Ok((llx, lly, urx, ury)) => {
                    let rot = self
                        .get_page_rotation(page_index)
                        .unwrap_or(0)
                        .rem_euclid(360);
                    matches!(rot, 90 | 180 | 270).then_some((rot, llx, lly, urx - llx, ury - lly))
                }
                Err(_) => None,
            };
        if let Some((rot, llx, lly, w, h)) = page_rotation {
            for s in spans.iter_mut() {
                // 90°/270°: only map runs whose own content is rotated; horizontal
                // content stays in raw user space (see rationale above).
                if rot != 180 && s.rotation_degrees == 0.0 {
                    continue;
                }
                Self::map_span_into_rotated_frame(s, rot, llx, lly, w, h);
            }
        }

        // Tategaki (vertical writing) intercept. Pages whose majority of
        // spans were emitted under WMode 1 (font /Encoding ends in -V or
        // the CMap declares /WMode 1) need right-to-left, top-to-bottom
        // ordering. Row-aware / XY-cut sorts assume horizontal flow and
        // scramble vertical text; per-span wmode lets us route just those
        // pages through a tategaki comparator while leaving every existing
        // horizontal corpus untouched.
        let vertical_count = spans.iter().filter(|s| s.wmode == 1).count();
        if !spans.is_empty() && vertical_count * 2 >= spans.len() {
            // See `crate::utils::sort_vertical_tategaki` for the
            // column-clustering algorithm and the total-order rationale.
            spans = crate::utils::sort_vertical_tategaki(spans, |s| &s.bbox);
        } else if let Some(ordered) = Self::sidebar_body_reading_order(&spans) {
            // RW-1: narrow-sidebar + wide-body first pages (full-width title band
            // over a metadata sidebar + body). Handled before the XY-cut so the
            // title is not sliced along the body gutter (§14.8.3).
            spans = ordered;
        } else if Self::is_multi_column_page(&spans) {
            use crate::pipeline::reading_order::{
                ReadingOrderContext as ROContext, ReadingOrderStrategy, XYCutStrategy,
            };
            let strategy = XYCutStrategy::new();
            let context = ROContext::new().with_page(page_index as u32);
            // Clone needed: apply() takes ownership, and the Err branch
            // falls back to sorting the original vec in place.
            match strategy.apply(spans.clone(), &context) {
                Ok(ordered) => {
                    spans = ordered.into_iter().map(|o| o.span).collect();
                }
                Err(e) => {
                    log::debug!(
                        "XY-cut reading order failed on page {page_index} ({e}), \
                         falling back to row-aware sort"
                    );
                    spans.sort_by(|a, b| {
                        crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
                    });
                    Self::reorder_rowspan_labels(&mut spans);
                }
            }
        } else {
            // Row-aware sort: Y-band descending (top→bottom), X ascending
            // within a row.
            spans.sort_by(|a, b| {
                crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
            });
            // Lift multi-row-spanning labels to the top of their block.
            Self::reorder_rowspan_labels(&mut spans);
        }

        // Per-span rotation firewall. Runs drawn with a rotated text matrix
        // (the vertical `arXiv:…` margin stamp, figure/axis labels, rotated
        // table headers, transit-poster route names) break the axis-aligned
        // row-band / XY-cut assumptions, so interleaving them with the
        // horizontal flow scrambles reading order. The reordering above ran on
        // the FULL span set (so its column/XY-cut decisions are unchanged — the
        // horizontal body keeps its exact baseline order); now stably lift the
        // rotated runs out (preserving horizontal order) and re-append them as
        // their own blocks, each ordered in an upright frame. No-op (and
        // byte-identical) when the page has no rotated spans.
        if spans.iter().any(|s| s.rotation_degrees != 0.0) {
            let rotated: Vec<crate::layout::TextSpan> = spans
                .iter()
                .filter(|s| s.rotation_degrees != 0.0)
                .cloned()
                .collect();
            spans.retain(|s| s.rotation_degrees == 0.0);
            spans.extend(Self::order_rotated_blocks(rotated));
        }

        // Append text from non-Widget annotations (/Subtype /Text, FreeText,
        // Stamp, Highlight, etc.) that carry a /Contents entry. These are not
        // part of the page content stream so they are not picked up by the
        // regular extractor. On a /Rotate'd page their /Rect-derived bboxes are
        // in unrotated page space, so map the appended spans into the same
        // displayed frame as the content spans (no-op for unrotated pages).
        // Annotation text is horizontal (rotation_degrees == 0), so on a 90°/270°
        // page it stays raw, matching the horizontal content spans above.
        let pre_annotation_len = spans.len();
        spans.extend(self.annotation_content_spans(page_index));
        if let Some((rot, llx, lly, w, h)) = page_rotation {
            for s in spans[pre_annotation_len..].iter_mut() {
                if rot != 180 && s.rotation_degrees == 0.0 {
                    continue;
                }
                Self::map_span_into_rotated_frame(s, rot, llx, lly, w, h);
            }
        }

        // Mark running headers/footers (untagged-PDF heuristic). Spans whose
        // normalized text recurs on >=50% of pages and sits near the top or
        // bottom of the page are flagged as artifacts so downstream filters
        // drop them.
        self.mark_running_artifact_spans(page_index, &mut spans)?;

        // Normalize Unicode typographic spaces (U+2000–U+200B, U+202F, U+205F)
        // to ASCII space. Some PDF producers encode word separators as hair-space
        // or thin-space variants in ToUnicode CMaps (e.g. justified text layouts);
        // normalising here gives consistent word boundaries to every downstream
        // consumer (extract_text, word-F1 scoring, etc.).
        for span in &mut spans {
            if span
                .text
                .chars()
                .any(|c| matches!(c, '\u{2000}'..='\u{200B}' | '\u{202F}' | '\u{205F}'))
            {
                span.text = span
                    .text
                    .chars()
                    .map(|c| {
                        if matches!(c, '\u{2000}'..='\u{200B}' | '\u{202F}' | '\u{205F}') {
                            ' '
                        } else {
                            c
                        }
                    })
                    .collect();
            }
        }

        // Apply char_widths boundary splits directly to span.text so that every
        // downstream consumer (to_markdown, to_html, extract_text) sees the same
        // word boundaries. extract_text applies the same logic through push_span_text;
        // after this normalization push_span_text sees a space at the boundary
        // becomes a no-op, so there is no double-application risk.
        for span in &mut spans {
            if let Some(split) = Self::char_widths_boundary_split(span) {
                let mut t = String::with_capacity(span.text.len() + 1);
                t.push_str(&span.text[..split]);
                t.push(' ');
                t.push_str(&span.text[split..]);
                span.text = t;
            }
        }

        // Detect superscript / subscript runs and substitute ASCII
        // digits with their Unicode super/sub-script equivalents
        // (only when the run is sandwiched between alphabetic body
        // spans on both sides — chemistry/math context like "S²X"
        // or "H₂O"). The same substitution would otherwise fire on
        // author-affiliation markers ("name¹,²") which the bench
        // ground truth keeps in ASCII; gating on token-internal
        // context keeps the desired cases without regressing the
        // affiliation-block pages.
        Self::apply_super_sub_script_substitutions(&mut spans);

        // Fold spacing-diacritic spans (´, `, ^, ~, ¨, …) into the
        // base letter of the following span when the diacritic is
        // centred over the base glyph. PDFs that pre-shape accented
        // Latin (LaTeX `\'E` → two glyphs, `acute` then `E`) emit
        // the marks as separate `Tj` ops at the base glyph's X
        // coordinate. Without this pass extract_text returns the
        // raw two-glyph order "´Ecole" instead of "École".
        Self::apply_combining_mark_composition(&mut spans);

        // Stamp accurate per-glyph x-origins onto the finalized spans so that
        // `to_chars()` (and thus extract_words / extract_spans /
        // extract_text_lines, which all decompose spans through it) reports
        // spec-aligned positions instead of drifting prefix-sums. Runs last, on
        // the fully post-processed spans, so alignment sees the same text the
        // consumers do.
        self.stamp_char_x_offsets(page_index, &mut spans);

        Ok(spans)
    }

    /// Copy the spec-aligned per-glyph baseline x-origins from the char-level
    /// extractor onto each span's [`char_x_offsets`](crate::layout::TextSpan::char_x_offsets).
    ///
    /// # Why
    ///
    /// [`TextSpan::to_chars`](crate::layout::TextSpan::to_chars) otherwise
    /// reconstructs each glyph's x by prefix-summing the span's nominal
    /// `char_widths` from `bbox.x`. Those nominal widths omit the ISO
    /// 32000-1:2008 §9.4.3 TJ-array adjustment (the number in a TJ array is
    /// "expressed in thousandths of a unit of text space … subtracted from the
    /// current … coordinate") and the full §9.4.4 text-space displacement
    /// (`t_x = ((w0 − Tj/1000) · Tfs + Tc + Tw) · Th`). Prefix-summing the
    /// nominal widths therefore drifts cumulatively along a line. The
    /// char-level extractor that [`extract_chars`](Self::extract_chars) uses
    /// implements §9.4.4 / §9.4.3 in full (it matches Poppler `pdftotext
    /// -bbox`), so its `origin_x` values are the authoritative positions this
    /// function stamps back onto the spans.
    ///
    /// # Alignment (robust, per span)
    ///
    /// A naive global greedy walk on char values mis-jumps on repeated
    /// letters / spaces. Instead, for each span we take only the accurate chars
    /// on the SAME baseline (`|origin_y − span.bbox.y| ≤ 0.5·font_size`), sort
    /// them by x, and match the span's glyph sequence as a CONTIGUOUS run,
    /// choosing the run whose first glyph's `origin_x` is nearest `span.bbox.x`.
    ///
    /// # Fallback (never guess)
    ///
    /// If a span cannot be fully, unambiguously aligned — no contiguous run of
    /// exactly the span's glyphs exists on its line (count mismatch from a
    /// post-processing text edit, ligature expansion, a synthetic space glyph
    /// not present in the char stream, …) — its `char_x_offsets` is left empty
    /// so `to_chars` uses the legacy prefix-sum path. A cleared span is a
    /// no-op, never a regression. `char_widths` is never touched (a downstream
    /// word-boundary heuristic keys off its length).
    ///
    /// 180° pages are skipped entirely: the spans are mirrored into the displayed
    /// frame while the accurate chars remain in unrotated page space, so a
    /// horizontal-x stamp would not correspond. On 90°/270° pages, horizontal
    /// content spans stay in raw user space and ARE stamped, but rotated-content
    /// spans (`rotation_degrees != 0`) have been mapped into the displayed frame
    /// (see `postprocess_spans`) and their glyphs run along a rotated axis, so a
    /// horizontal-x stamp would misalign — those individual spans are skipped.
    fn stamp_char_x_offsets(&self, page_index: usize, spans: &mut [crate::layout::TextSpan]) {
        // Horizontal-x offsets only make sense in an unrotated frame; the 180°
        // mirror is the one rotation that leaves ALL spans in the displayed frame.
        if self
            .get_page_rotation(page_index)
            .unwrap_or(0)
            .rem_euclid(360)
            == 180
        {
            return;
        }

        let accurate = match self.cached_page_chars(page_index) {
            Ok(chars) if !chars.is_empty() => chars,
            _ => return,
        };

        // Baseline index: char positions ordered by `origin_y`, so each span's
        // baseline slice is a binary-searched range rather than a linear scan
        // over every glyph on the page (that scan made this pass
        // O(spans x chars) — the dominant per-page cost on long documents).
        let mut by_y: Vec<u32> = (0..accurate.len() as u32).collect();
        by_y.sort_by(|&a, &b| {
            crate::utils::safe_float_cmp(
                accurate[a as usize].origin_y,
                accurate[b as usize].origin_y,
            )
        });
        let ys: Vec<f32> = by_y
            .iter()
            .map(|&i| accurate[i as usize].origin_y)
            .collect();

        for span in spans.iter_mut() {
            // Rotated-content spans are in the displayed frame (mapped on 90/270)
            // and their glyphs run vertically; a horizontal-x stamp from the raw
            // chars would not correspond, so leave them to the prefix-sum path.
            if span.rotation_degrees != 0.0 {
                continue;
            }
            // Start clean: any offsets carried over via struct-update from a
            // source span must not be trusted for this (possibly edited) text.
            span.char_x_offsets.clear();

            let glyphs: Vec<char> = span.text.chars().collect();
            let n = glyphs.len();
            if n == 0 {
                continue;
            }

            // Accurate chars sharing this span's baseline, left-to-right.
            let baseline_tol = 0.6 * span.font_size.max(1.0);
            // Chars sharing this span's baseline, left-to-right. The y-sorted
            // index brackets a candidate range; the exact `abs()` predicate then
            // selects from it, so the result matches a full linear scan even
            // where the bracket arithmetic rounds differently. The widened
            // bracket keeps that range a superset. Ordering by
            // (origin_x, source index) reproduces the previous stable
            // filter-then-sort: ties on origin_x keep their `accurate` order.
            let bracket = baseline_tol + baseline_tol.abs() * 1e-6 + f32::EPSILON;
            let lo = ys.partition_point(|&y| y < span.bbox.y - bracket);
            let hi = ys.partition_point(|&y| y <= span.bbox.y + bracket);
            let mut idx: Vec<u32> = by_y[lo..hi]
                .iter()
                .copied()
                .filter(|&i| (accurate[i as usize].origin_y - span.bbox.y).abs() <= baseline_tol)
                .collect();
            if idx.is_empty() {
                continue;
            }
            idx.sort_by(|&a, &b| {
                crate::utils::safe_float_cmp(
                    accurate[a as usize].origin_x,
                    accurate[b as usize].origin_x,
                )
                .then(a.cmp(&b))
            });
            let line: Vec<&crate::layout::TextChar> =
                idx.iter().map(|&i| &accurate[i as usize]).collect();

            // Greedy per-glyph alignment. Anchor the scan cursor at the accurate
            // char nearest this span's left edge, then walk the span's glyphs,
            // matching each to the next equal accurate char within a small
            // forward window. Unlike an all-or-nothing contiguous match, a single
            // unmatched glyph (an inserted word-boundary space, a ligature split,
            // a combining mark) no longer discards the whole span — such glyphs
            // are interpolated below so `char_x_offsets` still fills every index.
            let start = line
                .iter()
                .position(|c| c.origin_x >= span.bbox.x - 0.5)
                .unwrap_or(0);
            let mut assigned: Vec<Option<f32>> = vec![None; n];
            let mut li = start;
            for (k, &g) in glyphs.iter().enumerate() {
                let mut j = li;
                let mut steps = 0;
                while j < line.len() && steps < 6 {
                    if line[j].char == g {
                        assigned[k] = Some(line[j].origin_x);
                        li = j + 1;
                        break;
                    }
                    j += 1;
                    steps += 1;
                }
            }

            // Need enough real anchors to trust the run; otherwise fall back.
            let anchors = assigned.iter().filter(|a| a.is_some()).count();
            if anchors * 5 < n * 3 {
                // < 60% matched
                continue;
            }

            // Fill the gaps: each unmatched glyph takes the nearest preceding
            // anchor plus the prefix sum of the (locally accurate) char_widths
            // between them; if there is no preceding anchor, walk back from the
            // nearest following one. Over the short spans between anchors the
            // cumulative drift these interpolations reintroduce is sub-point.
            let cw = &span.char_widths;
            let width_at = |i: usize| -> f32 {
                if cw.len() == n {
                    cw[i]
                } else {
                    span.bbox.width / n as f32
                }
            };
            let mut offs = vec![0.0f32; n];
            // forward pass from preceding anchors
            let mut last: Option<(usize, f32)> = None;
            for k in 0..n {
                if let Some(x) = assigned[k] {
                    offs[k] = x;
                    last = Some((k, x));
                } else if let Some((lk, lx)) = last {
                    let acc: f32 = (lk..k).map(width_at).sum();
                    offs[k] = lx + acc;
                }
            }
            // backfill any leading None from the first following anchor
            if assigned[0].is_none() {
                if let Some(fk) = assigned.iter().position(|a| a.is_some()) {
                    let fx = assigned[fk].unwrap();
                    for k in 0..fk {
                        let acc: f32 = (k..fk).map(width_at).sum();
                        offs[k] = fx - acc;
                    }
                }
            }
            span.char_x_offsets = offs;
        }
    }

    /// Fold a one-char spacing-diacritic span into the following
    /// span's first character when they overlap in X (the typical
    /// LaTeX `\'E` → `(´)(E)` shape). Substitutes the relevant
    /// combining mark from U+0300..U+0327 and lets
    /// `unicode_normalization::nfc` precompose where it can
    /// ("E\u{0301}" → "É"). The diacritic span is left empty so
    /// downstream rendering skips it.
    fn apply_combining_mark_composition(spans: &mut Vec<crate::layout::TextSpan>) {
        use unicode_normalization::UnicodeNormalization;

        fn combining_for(spacing: char) -> Option<char> {
            Some(match spacing {
                '\u{00B4}' => '\u{0301}', // acute
                '\u{0060}' => '\u{0300}', // grave
                '\u{005E}' => '\u{0302}', // circumflex
                '\u{02C6}' => '\u{0302}', // modifier-letter circumflex
                '\u{007E}' => '\u{0303}', // tilde
                '\u{02DC}' => '\u{0303}', // small tilde
                '\u{00A8}' => '\u{0308}', // diaeresis
                '\u{00AF}' => '\u{0304}', // macron
                '\u{02C9}' => '\u{0304}', // modifier-letter macron
                '\u{00B8}' => '\u{0327}', // cedilla
                '\u{02DA}' => '\u{030A}', // ring above
                _ => return None,
            })
        }

        // First pass: spans that already got merged at the extractor
        // (when the LaTeX `(´)(Ecole)` pair both sit at the same
        // text-matrix origin the upstream merge_adjacent_spans pulls
        // them into a single "´Ecole" span). Fold the leading
        // diacritic + base letter into the precomposed form.
        for span in spans.iter_mut() {
            let mut iter = span.text.chars();
            let Some(d) = iter.next() else { continue };
            let Some(base) = iter.next() else { continue };
            let Some(combining) = combining_for(d) else {
                continue;
            };
            if !base.is_alphabetic() {
                continue;
            }
            let rest_start = d.len_utf8() + base.len_utf8();
            let mut composed = String::with_capacity(span.text.len() + 2);
            composed.push(base);
            composed.push(combining);
            composed.push_str(&span.text[rest_start..]);
            span.text = composed.nfc().collect();
        }

        // Walk spans pairwise. The diacritic is on its own one-
        // character span; the next span carries the base letter.
        let mut i = 0;
        while i + 1 < spans.len() {
            let mark_char = {
                let s = &spans[i];
                let mut iter = s.text.chars();
                let first = iter.next();
                let rest = iter.next();
                if rest.is_some() {
                    None
                } else {
                    first.and_then(combining_for)
                }
            };
            let Some(combining) = mark_char else {
                i += 1;
                continue;
            };
            // Geometric: same line, diacritic anchored over the base
            // letter's left edge (within ±1 pt).
            let (same_line, overlaps_x) = {
                let p = &spans[i];
                let n = &spans[i + 1];
                let same = (p.bbox.y - n.bbox.y).abs() < p.font_size.max(n.font_size) * 0.6;
                let dx = (p.bbox.x - n.bbox.x).abs();
                (same, dx <= 1.5)
            };
            if !(same_line && overlaps_x) {
                i += 1;
                continue;
            }
            // The next span must start with a base letter we can
            // attach a combining mark to (Latin letter / digit).
            let Some(base) = spans[i + 1].text.chars().next() else {
                i += 1;
                continue;
            };
            if !base.is_alphabetic() {
                i += 1;
                continue;
            }
            // Build "<base><combining><rest>" and NFC-compose.
            let mut composed = String::with_capacity(spans[i + 1].text.len() + 2);
            composed.push(base);
            composed.push(combining);
            let rest_start = base.len_utf8();
            composed.push_str(&spans[i + 1].text[rest_start..]);
            spans[i + 1].text = composed.nfc().collect();
            // Empty out the diacritic span; downstream consumers
            // skip zero-text spans.
            spans[i].text.clear();
            i += 2;
        }

        // Drop any spans we emptied.
        spans.retain(|s| !s.text.is_empty());
    }

    /// Substitute ASCII digits and a few punctuation characters in
    /// super/sub-script spans with their Unicode counterparts
    /// (U+2070..U+2079 / U+00B2/B3/B9 for superscripts,
    /// U+2080..U+2089 for subscripts). A span is treated as
    /// super- or sub-script when its font is meaningfully smaller
    /// than the previous span on the same line and its baseline is
    /// raised or lowered. Only spans whose text consists entirely
    /// of substitutable characters are rewritten — mixed-content
    /// or single-letter superscript callouts (e.g. footnote "a")
    /// fall through unchanged so the existing citation-handling
    /// path stays in control.
    fn apply_super_sub_script_substitutions(spans: &mut [crate::layout::TextSpan]) {
        fn super_for_char(c: char) -> Option<char> {
            Some(match c {
                '0' => '\u{2070}',
                '1' => '\u{00B9}',
                '2' => '\u{00B2}',
                '3' => '\u{00B3}',
                '4' => '\u{2074}',
                '5' => '\u{2075}',
                '6' => '\u{2076}',
                '7' => '\u{2077}',
                '8' => '\u{2078}',
                '9' => '\u{2079}',
                '+' => '\u{207A}',
                '-' => '\u{207B}',
                '=' => '\u{207C}',
                '(' => '\u{207D}',
                ')' => '\u{207E}',
                _ => return None,
            })
        }
        fn sub_for_char(c: char) -> Option<char> {
            Some(match c {
                '0' => '\u{2080}',
                '1' => '\u{2081}',
                '2' => '\u{2082}',
                '3' => '\u{2083}',
                '4' => '\u{2084}',
                '5' => '\u{2085}',
                '6' => '\u{2086}',
                '7' => '\u{2087}',
                '8' => '\u{2088}',
                '9' => '\u{2089}',
                '+' => '\u{208A}',
                '-' => '\u{208B}',
                '=' => '\u{208C}',
                '(' => '\u{208D}',
                ')' => '\u{208E}',
                _ => return None,
            })
        }
        // Two-pass: first compute the body-font baseline for each
        // line band (largest font_size on that line), then walk
        // spans and substitute any whose font is meaningfully
        // smaller AND whose baseline is raised or lowered relative
        // to the body baseline.
        let n = spans.len();
        if n < 2 {
            return;
        }
        const LINE_BAND_PT: f32 = 4.0;
        // band_anchor[i] = (body_font_size, body_y) of the line
        // band that span `i` belongs to. Sorting span indices by Y
        // once + sliding a two-pointer window over the sorted view
        // reduces the per-span band-anchor scan from O(n) to amortised
        // O(window_size), bringing the whole pass from O(n²) down to
        // O(n log n) on thesis-style pages with thousands of spans.
        let mut sorted_by_y: Vec<usize> = (0..n).collect();
        sorted_by_y
            .sort_by(|&a, &b| crate::utils::safe_float_cmp(spans[a].bbox.y, spans[b].bbox.y));
        let band_anchor = Self::compute_band_anchors(spans, &sorted_by_y, LINE_BAND_PT);
        // Spatial index: bucket spans by Y-band so `span_is_token_internal`
        // queries only nearby spans instead of all of them (its same-line
        // neighbour scan was O(n) per candidate → O(n²) on dense pages).
        let y_index = Self::build_y_band_index(spans, LINE_BAND_PT);
        for i in 0..n {
            let (anchor_fs, anchor_y) = band_anchor[i];
            let curr_fs = spans[i].font_size;
            // Skip the body span itself (it IS the anchor).
            if anchor_fs <= 0.0 || curr_fs >= anchor_fs * 0.85 {
                continue;
            }
            let y_delta = spans[i].bbox.y - anchor_y;
            let raised = y_delta > anchor_fs * 0.15;
            let lowered = y_delta < -anchor_fs * 0.15;
            if !raised && !lowered {
                continue;
            }
            let map: fn(char) -> Option<char> = if raised { super_for_char } else { sub_for_char };
            if spans[i].text.is_empty() || !spans[i].text.chars().all(|c| map(c).is_some()) {
                continue;
            }
            // Leave a signed numeric exponent (scientific unit notation such as
            // `s−1`, `m−2`) as ASCII. ToUnicode already decoded the intended
            // characters, and the plaintext convention every reference extractor
            // follows keeps these un-superscripted; rewriting `−1` to `₋₁` / `⁻¹`
            // is both wrong against that convention and — because the geometric
            // classifier fires inconsistently on borderline baselines — a source
            // of non-determinism across identical occurrences.
            if Self::run_is_signed_number(&spans[i].text) {
                continue;
            }
            // Limit the substitution to clearly token-internal
            // super/sub-scripts: the run must have a base-sized
            // neighbour on BOTH sides whose first/last char is
            // alphabetic and roughly adjacent in X. Author-
            // affiliation markers like "name¹,²" sit at the END
            // of a line with no following body letter; the bench
            // GT renders those as plain ASCII digits, so substi-
            // tuting them would regress. Restricting to sandwiched
            // runs keeps the chemistry / exponent cases that the
            // GT does want as Unicode (S², H₂O, k₁) and skips the
            // trailing footnote callouts.
            if !Self::span_is_token_internal(spans, i, &y_index, LINE_BAND_PT) {
                continue;
            }
            let substituted: String = spans[i].text.chars().map(|c| map(c).unwrap()).collect();
            spans[i].text = substituted;
        }
    }

    /// A run is a signed numeric exponent — e.g. `-1`, `−2`, `‑3` — when it
    /// opens with a minus/hyphen sign and contains at least one digit. Such runs
    /// are scientific unit exponents (`s−1`, `m−2`) that the plaintext extraction
    /// convention keeps as ASCII, so [`apply_super_sub_script_substitutions`]
    /// must not rewrite them into Unicode sub/superscript glyphs.
    ///
    /// [`apply_super_sub_script_substitutions`]: Self::apply_super_sub_script_substitutions
    fn run_is_signed_number(text: &str) -> bool {
        let is_minus = |c: char| matches!(c, '\u{002D}' | '\u{2212}' | '\u{2010}' | '\u{2011}');
        matches!(text.chars().next(), Some(c) if is_minus(c))
            && text.chars().any(|c| c.is_ascii_digit())
    }

    /// For every span, the `(max_font_size, anchor_y)` over the spans within
    /// `±band` of its Y, in O(n) via a sliding-window maximum (monotonic deque)
    /// over the Y-sorted order. Replaces a per-span window walk that was O(n²)
    /// when many spans share a Y band (wide table rows).
    ///
    /// Tie-break on equal max font size: the lowest-Y span (deque keeps the
    /// earliest sorted position). A substitution only fires when the span's own
    /// font is strictly smaller than the anchor, so the tie-break merely picks
    /// which equal-sized body span supplies `anchor_y`, all within `band`.
    fn compute_band_anchors(
        spans: &[crate::layout::TextSpan],
        sorted_by_y: &[usize],
        band: f32,
    ) -> Vec<(f32, f32)> {
        let n = sorted_by_y.len();
        let mut band_anchor = vec![(0.0f32, 0.0f32); n];
        let y = |p: usize| spans[sorted_by_y[p]].bbox.y;
        let fs = |p: usize| spans[sorted_by_y[p]].font_size;
        // Deque of sorted positions, font size non-increasing front→back;
        // positions are pushed in increasing order so the deque is also
        // position-increasing front→back (front = smallest position = max fs).
        let mut deque: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        let mut lo = 0usize;
        let mut hi = 0usize;
        for pos in 0..n {
            let cy = y(pos);
            while hi < n && y(hi) <= cy + band {
                while let Some(&back) = deque.back() {
                    if fs(back) < fs(hi) {
                        deque.pop_back();
                    } else {
                        break;
                    }
                }
                deque.push_back(hi);
                hi += 1;
            }
            while lo < n && y(lo) < cy - band {
                if deque.front() == Some(&lo) {
                    deque.pop_front();
                }
                lo += 1;
            }
            let best = *deque.front().expect("window always contains pos");
            band_anchor[sorted_by_y[pos]] = (fs(best), y(best));
        }
        band_anchor
    }

    /// Return true when span `i` has a base-sized alphabetic
    /// neighbour both before and after it on the same line band,
    /// within ~1 em horizontally. That captures the "X²Y" /
    /// "H₂O" / "k₁ + …" pattern but excludes footnote markers
    /// that hang off the end of a word with no following body
    /// character.
    /// Bucket span indices by Y-band (`round(y / band)`) so same-line lookups
    /// scan only nearby bands instead of every span. Querying a band `k`'s
    /// `[k-2, k+2]` neighbours is a guaranteed superset of all spans within
    /// `band` points of any Y in band `k`, so an exact `|Δy|` filter on the
    /// result is byte-identical to a full scan.
    fn build_y_band_index(
        spans: &[crate::layout::TextSpan],
        band: f32,
    ) -> HashMap<i32, Vec<usize>> {
        let mut idx: HashMap<i32, Vec<usize>> = HashMap::new();
        for (j, s) in spans.iter().enumerate() {
            idx.entry((s.bbox.y / band).round() as i32)
                .or_default()
                .push(j);
        }
        idx
    }

    /// Indices in the Y-bands within ±2 of `y`'s band (superset of `|Δy| ≤ band`).
    fn y_band_candidates<'a>(
        y_index: &'a HashMap<i32, Vec<usize>>,
        y: f32,
        band: f32,
    ) -> impl Iterator<Item = usize> + 'a {
        let k = (y / band).round() as i32;
        (k - 2..=k + 2).flat_map(move |b| y_index.get(&b).into_iter().flatten().copied())
    }

    fn span_is_token_internal(
        spans: &[crate::layout::TextSpan],
        i: usize,
        y_index: &HashMap<i32, Vec<usize>>,
        band: f32,
    ) -> bool {
        let curr = &spans[i];
        let curr_y = curr.bbox.y;
        let curr_x = curr.bbox.x;
        let curr_right = curr.bbox.x + curr.bbox.width;
        let body_fs = Self::y_band_candidates(y_index, curr_y, band)
            .filter(|&j| (spans[j].bbox.y - curr_y).abs() <= 4.0)
            .map(|j| spans[j].font_size)
            .fold(0f32, f32::max)
            .max(1.0);
        let neighbour_fs_min = body_fs * 0.85;
        let max_em = body_fs;
        let mut has_left = false;
        let mut has_right = false;
        for j in Self::y_band_candidates(y_index, curr_y, band) {
            if j == i {
                continue;
            }
            let s = &spans[j];
            if (s.bbox.y - curr_y).abs() > 4.0 {
                continue;
            }
            if s.font_size < neighbour_fs_min {
                continue;
            }
            // Anchor must start or end with an alphabetic character
            // — a digit or punctuation neighbour does not signal a
            // token-internal context.
            let s_right = s.bbox.x + s.bbox.width;
            // Allow small overlap (super/sub glyphs nest slightly
            // under the body letter's bounding box).
            let dx_left = curr_x - s_right;
            if s_right < curr_right
                && dx_left <= max_em
                && dx_left >= -max_em * 0.5
                && s.text
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphabetic())
            {
                has_left = true;
            }
            let dx_right = s.bbox.x - curr_right;
            if s.bbox.x > curr_x
                && dx_right <= max_em
                && dx_right >= -max_em * 0.5
                && s.text.chars().next().is_some_and(|c| c.is_alphabetic())
            {
                has_right = true;
            }
        }
        has_left && has_right
    }

    /// Return per-page font statistics for use in heading detection and layout analysis.
    ///
    /// [`crate::layout::PageFontStats`] contains:
    /// - `dominant_em`: the mode font size weighted by character count — the body text "1 em"
    /// - `dominant_line_height`: median baseline-to-baseline distance
    /// - `dominant_char_width`: average character advance width
    /// - `body_font_name`: name of the most-used font
    ///
    /// The primary use-case is heading detection in downstream tools: compare
    /// `span.font_size / stats.dominant_em` against a threshold (e.g. 1.4×
    /// for H2, 1.8× for H1) to classify large-font spans as headings without
    /// depending on any hardcoded point sizes.
    ///
    /// ```ignore
    /// let stats = doc.page_font_stats(0)?;
    /// let spans = doc.extract_spans(0)?;
    /// for span in &spans {
    ///     let ratio = span.font_size / stats.dominant_em;
    ///     if ratio >= 1.8 { println!("H1: {}", span.text); }
    ///     else if ratio >= 1.4 { println!("H2: {}", span.text); }
    /// }
    /// ```
    pub fn page_font_stats(&self, page_index: usize) -> Result<crate::layout::PageFontStats> {
        let spans = self.extract_spans(page_index)?;
        Ok(crate::layout::PageFontStats::from_spans(&spans))
    }

    /// Return all extraction warnings accumulated since this document was opened.
    ///
    /// Warnings are recorded when silent fallbacks occur during text extraction
    /// (e.g., missing ToUnicode CMap, font not found, malformed structure tree).
    /// They do NOT consume the warning list — use [`Self::take_warnings`] to drain it.
    ///
    /// This API makes previously invisible extraction degradations programmatically
    /// observable without requiring callers to hook into the `log` crate.
    pub fn warnings(&self) -> Vec<String> {
        self.accumulated_warnings.lock_or_recover().clone()
    }

    /// Drain and return all accumulated extraction warnings, clearing the list.
    ///
    /// After this call, [`Self::warnings`] returns an empty `Vec` until new warnings
    /// are generated. Useful for incremental processing pipelines that want to
    /// inspect warnings on a per-page or per-operation basis.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.accumulated_warnings.lock_or_recover())
    }

    /// Record an extraction warning. Called internally when a silent fallback occurs.
    pub(crate) fn push_warning(&self, msg: impl Into<String>) {
        self.accumulated_warnings.lock_or_recover().push(msg.into());
    }

    /// Return the document's accumulated structured warnings as a
    /// snapshot. Each entry carries the warning's
    /// [`WarningCategory`](crate::extractors::warnings::WarningCategory),
    /// page (if applicable), human-readable message, and PDF spec
    /// section reference (when applicable).
    ///
    /// Unlike [`Self::warnings`] which returns plain strings, this
    /// accessor returns structured records callers can filter, route
    /// to observability dashboards, or assert on in tests without
    /// parsing message text. Pairs with the `pyo3_log` per-target
    /// default-level downgrade to give Python users a clean stderr
    /// experience plus an opt-in structured surface.
    ///
    /// Returns the warnings in insertion order. The vector is
    /// non-destructive: subsequent calls return the same entries
    /// plus any new ones pushed since the last call. Use
    /// [`Self::take_structured_warnings`] to drain.
    ///
    /// Merges the process-wide `GLOBAL_WARNING_SINK` (where
    /// free-function log sites like `SPEC VIOLATION`,
    /// operator-cap-exceeded, and Type0/Type3 font fallbacks push
    /// their structured records) into the per-document sink on each
    /// call. The drain attribution follows the "first caller wins"
    /// rule documented at the global sink — process-wide scope means
    /// the first document to call `structured_warnings` collects
    /// the global tail that accumulated since the last drain.
    ///
    /// Renamed from `flatten_warnings` in to avoid colliding
    /// with the pre-existing `DocumentEditor::flatten_warnings`
    /// (which returns the form-flattening side-effect log, a
    /// `&[String]` — different feature). Both the Rust and Python
    /// (`PyDocument`) surfaces now agree on `structured_warnings`.
    pub fn structured_warnings(&self) -> Vec<crate::extractors::warnings::Warning> {
        let global = crate::extractors::warnings::drain_global_warnings();
        if !global.is_empty() {
            self.warning_sink.extend(global);
        }
        self.warning_sink.snapshot()
    }

    /// Drain and return all accumulated structured warnings.
    /// Companion to [`Self::structured_warnings`].
    pub fn take_structured_warnings(&self) -> Vec<crate::extractors::warnings::Warning> {
        self.warning_sink.take()
    }

    /// Record a structured warning. Hook called from migrated
    /// `log::warn!` sites that also want to surface the warning as
    /// structured data.
    ///
    /// Exposed as `pub` so external diagnostic sources (custom
    /// extractors, FFI hooks) can also push warnings into the same
    /// sink that [`Self::structured_warnings`] surfaces.
    pub fn push_structured_warning(&self, warning: crate::extractors::warnings::Warning) {
        self.warning_sink.push(warning);
    }

    /// Heuristic: does this page have two or more vertical text columns?
    ///
    /// Used by `extract_spans` to decide whether to pay the XY-cut cost
    /// (correct but slower on large pages) or stick with the cheap row-
    /// aware sort. The check bins span X-centers into a small histogram
    /// and looks for two dense bands separated by a gutter whose spans
    /// vertically overlap with each other — that's the defining shape
    /// of a multi-column layout (newspaper / academic / dashboard) as
    /// opposed to sparse side-notes that flank a single column.
    ///
    /// False negatives (missed multi-column page) just mean we use the
    /// old reading order. False positives (single column routed through
    /// XY-cut) cost a bit of CPU but produce the same or better result.
    /// Both sides degrade gracefully.
    /// True when the page splits into side-by-side columns separated by a clean
    /// vertical gutter that no text span crosses.
    ///
    /// This is the small-page companion to the histogram detector in
    /// [`Self::is_multi_column_page`]: a two-column page with only a handful of
    /// wrapped lines per column (a short article, a synthetic fixture) carries
    /// too few spans for a projection histogram to classify, yet the gutter is
    /// perfectly unambiguous. We recover it directly:
    ///
    /// 1. Drop spans whose width exceeds 60 % of the content width — full-bleed
    ///    headings/footers legitimately straddle the gutter and must not veto it
    ///    (the recursive XY-Cut handles them with a horizontal cut first).
    /// 2. Sweep the remaining boxes left-to-right merging their X extents; a
    ///    forward jump of ≥ `MIN_GUTTER_PT` between the running right edge and
    ///    the next box's left edge is an empty channel that no span crosses.
    /// 3. Accept only when ≥ 2 spans sit on each side (genuine columns, not a
    ///    stray indent or page number) and the two sides' vertical ranges
    ///    overlap (columns sit beside each other, ruling out stacked blocks).
    fn has_clean_column_gutter(spans: &[crate::layout::TextSpan]) -> bool {
        /// Minimum empty-channel width. Real column gutters run ≥ 18pt; ordinary
        /// inter-word/inter-cell gaps are both narrower and crossed by spans on
        /// other lines, so they never survive the sweep.
        const MIN_GUTTER_PT: f32 = 18.0;

        // (x0, x1, y0, y1) for every non-empty, finite span.
        let mut boxes: Vec<(f32, f32, f32, f32)> = spans
            .iter()
            .filter(|s| {
                !s.text.trim().is_empty()
                    && s.bbox.x.is_finite()
                    && s.bbox.y.is_finite()
                    && s.bbox.width.is_finite()
                    && s.bbox.height.is_finite()
                    && s.bbox.width > 0.0
            })
            .map(|s| {
                (
                    s.bbox.x,
                    s.bbox.x + s.bbox.width,
                    s.bbox.y,
                    s.bbox.y + s.bbox.height,
                )
            })
            .collect();
        if boxes.len() < 4 {
            return false;
        }

        let content_min_x = boxes.iter().map(|b| b.0).fold(f32::INFINITY, f32::min);
        let content_max_x = boxes.iter().map(|b| b.1).fold(f32::NEG_INFINITY, f32::max);
        let content_w = content_max_x - content_min_x;
        if content_w < 100.0 {
            return false; // a single narrow column cannot hold a gutter
        }

        // Exclude full-width headings/footers from the gutter sweep.
        boxes.retain(|b| (b.1 - b.0) <= 0.6 * content_w);
        if boxes.len() < 4 {
            return false;
        }
        boxes.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));

        // Grid-row guard. A two-column body has exactly one text run per column
        // on a line, so a row carries at most one wide internal gap (the
        // gutter). A table / form row instead has several cells, i.e. two or
        // more wide internal gaps. Group the (heading-excluded) boxes into rows
        // by baseline and, when the majority of multi-box rows carry ≥ 2
        // significant internal gaps, treat the page as a grid and bail — a
        // single wide middle gap on a 2×N cell grid would otherwise read as a
        // lone gutter. Mirrors the grid-row discriminator on the histogram path.
        const MIN_GAP_PT: f32 = 6.0;
        let mut rows: std::collections::BTreeMap<i32, Vec<(f32, f32)>> =
            std::collections::BTreeMap::new();
        for &(x0, x1, _y0, y1) in &boxes {
            rows.entry(y1.round() as i32).or_default().push((x0, x1));
        }
        let (mut multi_gap_rows, mut counted_rows) = (0usize, 0usize);
        for cells in rows.values() {
            if cells.len() < 2 {
                continue;
            }
            let mut s = cells.clone();
            s.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
            let gaps = s
                .windows(2)
                .filter(|w| w[1].0 - w[0].1 >= MIN_GAP_PT)
                .count();
            counted_rows += 1;
            if gaps >= 2 {
                multi_gap_rows += 1;
            }
        }
        if counted_rows > 0 && multi_gap_rows * 2 >= counted_rows {
            return false; // grid / form, not a two-column body
        }

        // Sweep-merge X extents and collect EVERY ≥ MIN_GUTTER_PT forward jump
        // (a vertical channel no span crosses). A genuine two-column body has
        // exactly ONE such corridor — the gutter; the lines inside each column
        // overlap horizontally, so a column's own extents merge into a single
        // contiguous run. A short-cell table (numeric grid, form) instead leaves
        // a corridor between every cell column, so two or more qualifying gaps
        // means a grid / multi-region layout, not two columns — reject it
        // (matching the grid-row discriminator on the histogram path).
        let mut cover_right = boxes[0].1;
        let mut gutter_splits: Vec<usize> = Vec::new();
        for i in 1..boxes.len() {
            if boxes[i].0 - cover_right >= MIN_GUTTER_PT {
                gutter_splits.push(i);
            }
            cover_right = cover_right.max(boxes[i].1);
        }
        if gutter_splits.len() != 1 {
            return false; // 0 = single column; ≥ 2 = grid / multi-region
        }

        let (left, right) = boxes.split_at(gutter_splits[0]);
        if left.len() < 2 || right.len() < 2 {
            return false;
        }
        // Vertical ranges of the two sides must overlap — otherwise the
        // "columns" are vertically stacked blocks (e.g. a body block above a
        // sidebar), which read fine row-aware.
        let l_y0 = left.iter().map(|b| b.2).fold(f32::INFINITY, f32::min);
        let l_y1 = left.iter().map(|b| b.3).fold(f32::NEG_INFINITY, f32::max);
        let r_y0 = right.iter().map(|b| b.2).fold(f32::INFINITY, f32::min);
        let r_y1 = right.iter().map(|b| b.3).fold(f32::NEG_INFINITY, f32::max);
        let overlap = l_y1.min(r_y1) - l_y0.max(r_y0);
        let min_height = (l_y1 - l_y0).min(r_y1 - r_y0);
        min_height > 0.0 && overlap > 0.5 * min_height
    }

    /// Gutter X for a page that is genuinely **two-column PROSE** (#734), or
    /// `None`. Content-balance discriminator (corpus-measured): rejects forms
    /// (`label:value`), TOCs (`title…page#`), tables and N-up — all of which
    /// share a clean gutter but must read row-wise. A real two-column body has
    /// full-length text on both sides of the gutter.
    /// Measure a single central vertical gutter (a column-separating whitespace
    /// corridor) as a PURE geometric read. Returns the gutter's mid-X when the
    /// page has EXACTLY ONE corridor ≥ `MIN_GUTTER_PT` wide near mid-page
    /// (`0.30..=0.70` of content width); `None` for single-column, multi-corridor
    /// (grid/table/form), off-centre, or too-narrow pages — so a caller that
    /// gates on `Some` is byte-identical on all of those.
    ///
    /// Shared by the marginalia pre-filter (Item 2) and the topological
    /// union-find gutter veto (Item 4). Deliberately NOT a refactor of
    /// `prose_two_column_gutter` / `has_clean_column_gutter`: those use different
    /// corridor thresholds (12 / 18) and additional structural guards, and
    /// unifying them is high blast radius for no benefit. This is a separate,
    /// conservative 18 pt central-corridor probe.
    #[allow(dead_code)] // wired by Items 2 (P3) and 4 (P4)
    fn measure_single_central_gutter(spans: &[crate::layout::TextSpan]) -> Option<f32> {
        const MIN_GUTTER_PT: f32 = 18.0;
        let body: Vec<&crate::layout::TextSpan> = spans
            .iter()
            .filter(|s| {
                !s.text.trim().is_empty()
                    && s.bbox.width > 0.0
                    && s.bbox.x.is_finite()
                    && s.bbox.width.is_finite()
            })
            .collect();
        if body.len() < 8 {
            return None;
        }
        let cmin = body.iter().map(|s| s.bbox.x).fold(f32::INFINITY, f32::min);
        let cmax = body
            .iter()
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        let content_w = cmax - cmin;
        if content_w < 100.0 {
            return None;
        }
        // Exclude full-width spanning rows (headings/footers) so they don't mask
        // the corridor (same exclusion as the prose/clean-gutter sweeps).
        let mut boxes: Vec<(f32, f32)> = body
            .iter()
            .filter(|s| s.bbox.width <= 0.6 * content_w)
            .map(|s| (s.bbox.x, s.bbox.x + s.bbox.width))
            .collect();
        if boxes.len() < 8 {
            return None;
        }
        boxes.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
        let mut cover = boxes[0].1;
        let (mut corridors, mut gutter_x) = (0usize, 0.0f32);
        for &(l, r) in &boxes[1..] {
            if l - cover >= MIN_GUTTER_PT {
                corridors += 1;
                gutter_x = (cover + l) * 0.5;
            }
            cover = cover.max(r);
        }
        if corridors != 1 || !(0.30..=0.70).contains(&((gutter_x - cmin) / content_w)) {
            return None;
        }
        Some(gutter_x)
    }

    /// Valley-DEPTH central gutter probe. Like `measure_single_central_gutter`,
    /// returns the mid-X of a single central column-separating corridor, but uses
    /// a 2-D span-PROJECTION density (the emptiest vertical channel over the whole
    /// Y-extent) instead of a 1-D running-cover scan. This finds gutters that the
    /// cover scan misses because a full-width header/footer that is NOT quite wide
    /// enough to be band-excluded (it spans, say, 0.55 of the content width) jumps
    /// the running cover past the corridor; the projection only counts spans that
    /// actually straddle a given x, so a single bridging line is absorbed by the
    /// tolerance. It also catches the TIGHT (≈ 10–14 pt) real gutters of dense
    /// two-column journal bodies, below the conservative 18 pt cover threshold.
    ///
    /// A true gutter is a vertical band of near-zero straddle density; a phantom
    /// word/indent gap has moderate density (many lines carry text there). Returns
    /// the gutter mid-X only when EXACTLY ONE such near-empty central corridor of
    /// real width exists; `None` otherwise. Used (OR-ed with the cover scan) as
    /// the topological union-find gutter veto, so it can only PREVENT a
    /// cross-gutter union — never create one — keeping non-2-column pages
    /// byte-identical.
    fn density_central_gutter(spans: &[crate::layout::TextSpan]) -> Option<f32> {
        let finite = |s: &crate::layout::TextSpan| {
            !s.text.trim().is_empty()
                && s.bbox.width > 0.0
                && s.bbox.x.is_finite()
                && s.bbox.width.is_finite()
        };
        let body: Vec<&crate::layout::TextSpan> = spans.iter().filter(|s| finite(s)).collect();
        if body.len() < 12 {
            return None;
        }
        let cmin = body.iter().map(|s| s.bbox.x).fold(f32::INFINITY, f32::min);
        let cmax = body
            .iter()
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        let content_w = cmax - cmin;
        if !content_w.is_finite() || content_w < 100.0 {
            return None;
        }
        // Column-content spans only (exclude true full-width bands). A real
        // gutter is invisible under titles/abstracts/footers, so they must not
        // count toward straddle density.
        let band_w = 0.6 * content_w;
        let cols: Vec<(f32, f32)> = body
            .iter()
            .filter(|s| s.bbox.width <= band_w)
            .map(|s| (s.bbox.x, s.bbox.x + s.bbox.width))
            .collect();
        if cols.len() < 12 {
            return None;
        }
        // Scan the central band; "empty" tolerates ~1 % stray straddlers (a rare
        // long token or a header just under the band-exclusion width).
        let lo = cmin + 0.30 * content_w;
        let hi = cmin + 0.70 * content_w;
        let step = (content_w / 400.0).clamp(0.5, 3.0);
        let empty_max = (0.01 * cols.len() as f32).ceil() as usize;
        let straddle_at = |x: f32| -> usize {
            cols.iter()
                .filter(|(l, r)| *l + 2.0 < x && *r - 2.0 > x)
                .count()
        };
        // Find ALL near-empty corridors and the widest one; require EXACTLY ONE
        // (a 3-column grid has two, and must stay row-aware).
        let (mut corridors, mut best_w, mut best_mid) = (0usize, 0.0f32, f32::NAN);
        let (mut run_start, mut in_run) = (lo, false);
        let mut x = lo;
        let close = |run_start: f32,
                     end: f32,
                     corridors: &mut usize,
                     best_w: &mut f32,
                     best_mid: &mut f32| {
            let w = end - run_start;
            if w >= 6.0 {
                *corridors += 1;
                if w > *best_w {
                    *best_w = w;
                    *best_mid = (run_start + end) * 0.5;
                }
            }
        };
        while x <= hi {
            if straddle_at(x) <= empty_max {
                if !in_run {
                    run_start = x;
                    in_run = true;
                }
            } else if in_run {
                close(run_start, x, &mut corridors, &mut best_w, &mut best_mid);
                in_run = false;
            }
            x += step;
        }
        if in_run {
            close(run_start, hi, &mut corridors, &mut best_w, &mut best_mid);
        }
        if corridors != 1 || !best_mid.is_finite() {
            return None;
        }
        // Balanced columns: each side carries a real share of the column spans
        // (rejects a single column beside a sparse margin rail).
        let (mut left, mut right) = (0usize, 0usize);
        for (l, r) in &cols {
            if (l + r) * 0.5 < best_mid {
                left += 1;
            } else {
                right += 1;
            }
        }
        let n = left + right;
        if n == 0 || (left * 4 < n) || (right * 4 < n) {
            return None;
        }
        Some(best_mid)
    }

    /// Characters-per-text-line density for a set of spans (≈ chars per line).
    /// Lines are counted by clustering span upper edges (`bbox.bottom()`, larger
    /// y) with a `med_h * 0.6` gap. A page-number rail or a form's value column
    /// is text-SPARSE (a few chars per line); genuine prose columns and metadata
    /// sidebars are text-DENSE. Shared by the topological side-by-side gate
    /// (Item 1) and the marginalia sparsity gate (Item 2) — same formula the
    /// `topological_block_order` `char_density` closure uses.
    #[allow(dead_code)] // wired by Items 1 (P4) and 2 (P3)
    fn block_char_density(spans: &[&crate::layout::TextSpan], med_h: f32) -> f32 {
        if spans.is_empty() {
            return 0.0;
        }
        let med_h = med_h.max(1.0);
        let mut ys: Vec<f32> = spans.iter().map(|s| s.bbox.bottom()).collect();
        ys.sort_by(|p, q| crate::utils::safe_float_cmp(*p, *q));
        let mut lines = 1usize;
        for w in ys.windows(2) {
            if (w[1] - w[0]).abs() > med_h * 0.6 {
                lines += 1;
            }
        }
        let chars: usize = spans.iter().map(|s| s.text.trim().chars().count()).sum();
        chars as f32 / lines as f32
    }

    /// Detect a marginalia column (Item 2 / M2): a narrow, sparse, body-aligned
    /// numeric rail at the extreme left or right of the page — manuscript line
    /// numbers (`118 119 120 …`), a folio rail. Returns the indices of the rail
    /// spans (into `spans`) so the caller can lift them OUT of the body before
    /// geometric column dispatch (a rail otherwise injects a spurious second
    /// corridor / sparse block that disqualifies prose/topo detection) and
    /// re-append them at the end of the reading order.
    ///
    /// Tight 7-gate conjunction so it is a strict no-op (`None`) on ordinary
    /// pages and never lifts a genuine narrow first column (which is text-DENSE
    /// and multi-word → fails the sparsity + numeric-shape gates). `None` keeps
    /// the caller byte-identical.
    fn lift_marginalia_column(spans: &[crate::layout::TextSpan]) -> Option<Vec<usize>> {
        use crate::utils::safe_float_cmp;
        // Gate 1: a substantial multi-span body page.
        let texties: Vec<usize> = (0..spans.len())
            .filter(|&i| {
                !spans[i].text.trim().is_empty()
                    && spans[i].bbox.x.is_finite()
                    && spans[i].bbox.width.is_finite()
                    && spans[i].bbox.width > 0.0
            })
            .collect();
        if texties.len() < 12 {
            return None;
        }
        let median = |mut v: Vec<f32>| -> Option<f32> {
            if v.is_empty() {
                return None;
            }
            v.sort_by(|a, b| safe_float_cmp(*a, *b));
            Some(v[v.len() / 2].max(1.0))
        };
        let med_fs = median(
            texties
                .iter()
                .filter(|&&i| spans[i].text.trim().chars().count() >= 2 && spans[i].font_size > 0.0)
                .map(|&i| spans[i].font_size)
                .collect(),
        )?;
        let med_h = median(
            texties
                .iter()
                .map(|&i| spans[i].bbox.height.abs())
                .filter(|h| h.is_finite() && *h > 0.0)
                .collect(),
        )?;
        let cmin = texties
            .iter()
            .map(|&i| spans[i].bbox.x)
            .fold(f32::INFINITY, f32::min);
        let cmax = texties
            .iter()
            .map(|&i| spans[i].bbox.x + spans[i].bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        let content_w = cmax - cmin;
        if content_w < 100.0 {
            return None;
        }
        let xband = 3.0 * med_fs; // Gate 2: narrow strip width.

        // M2 targets LEFT-margin manuscript line-number rails (the documented
        // mechanism — "narrow left-margin numerals woven into the prose stream").
        // A right-margin narrow numeric column is predominantly a TOC/table-of-
        // contents PAGE-NUMBER reference that pairs 1:1 with its entry row;
        // lifting it would regroup the page numbers away from their entries (a
        // reorder that hurts TOC pages, observed on CFR Title 36). So only the
        // left rail is considered. The symmetric right-side geometry below is
        // retained so a future TOC-discriminating gate can re-enable it safely.
        for left_side in core::iter::once(true) {
            let in_band = |i: usize| -> bool {
                let l = spans[i].bbox.x;
                let r = spans[i].bbox.x + spans[i].bbox.width;
                if left_side {
                    r <= cmin + xband
                } else {
                    l >= cmax - xband
                }
            };
            let strip: Vec<usize> = texties.iter().copied().filter(|&i| in_band(i)).collect();
            if strip.len() < 3 {
                continue;
            }
            let strip_set: std::collections::HashSet<usize> = strip.iter().copied().collect();
            let body: Vec<usize> = texties
                .iter()
                .copied()
                .filter(|i| !strip_set.contains(i))
                .collect();
            if body.len() < 8 {
                continue; // need a real body to order around the rail
            }

            // Gate 3: SPARSE (few chars per line).
            let strip_refs: Vec<&crate::layout::TextSpan> =
                strip.iter().map(|&i| &spans[i]).collect();
            if Self::block_char_density(&strip_refs, med_h) >= 4.0 {
                continue;
            }

            // Gate 7: at least 3 rail lines (a recurring rail, not a stray number).
            let mut ys: Vec<f32> = strip.iter().map(|&i| spans[i].bbox.bottom()).collect();
            ys.sort_by(|p, q| safe_float_cmp(*p, *q));
            let lines = 1 + ys
                .windows(2)
                .filter(|w| (w[1] - w[0]).abs() > med_h * 0.6)
                .count();
            if lines < 3 {
                continue;
            }

            // Gate 6: NUMERIC-SHAPE — ≥70% pure digits or ≤3-char tokens. This is
            // the discriminator vs a real narrow prose column (multi-word lines).
            let numeric = strip
                .iter()
                .filter(|&&i| {
                    let t = spans[i].text.trim();
                    (!t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
                        || t.chars().count() <= 3
                })
                .count();
            if (numeric as f32) < 0.70 * strip.len() as f32 {
                continue;
            }

            // Gate 4: DETACHED — a clear ≥18 pt empty gutter between the rail's
            // inner edge and the body's outer edge (the rail is geometrically
            // separate, not just the first words of body lines).
            let (strip_inner, body_outer) = if left_side {
                (
                    strip
                        .iter()
                        .map(|&i| spans[i].bbox.x + spans[i].bbox.width)
                        .fold(f32::NEG_INFINITY, f32::max),
                    body.iter()
                        .map(|&i| spans[i].bbox.x)
                        .fold(f32::INFINITY, f32::min),
                )
            } else {
                (
                    strip
                        .iter()
                        .map(|&i| spans[i].bbox.x)
                        .fold(f32::INFINITY, f32::min),
                    body.iter()
                        .map(|&i| spans[i].bbox.x + spans[i].bbox.width)
                        .fold(f32::NEG_INFINITY, f32::max),
                )
            };
            let gutter = if left_side {
                body_outer - strip_inner
            } else {
                strip_inner - body_outer
            };
            if gutter < 18.0 {
                continue;
            }

            // Gate 5: BODY-ALIGNED — the rail runs ALONGSIDE the body (Y-overlap
            // > half the rail height), not above/below it.
            let (sy0, sy1) = strip
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &i| {
                    (
                        a.min(spans[i].bbox.y),
                        b.max(spans[i].bbox.y + spans[i].bbox.height),
                    )
                });
            let (by0, by1) = body
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &i| {
                    (
                        a.min(spans[i].bbox.y),
                        b.max(spans[i].bbox.y + spans[i].bbox.height),
                    )
                });
            let overlap = sy1.min(by1) - sy0.max(by0);
            if overlap <= 0.5 * (sy1 - sy0).max(1.0) {
                continue;
            }

            return Some(strip);
        }
        None
    }

    fn prose_two_column_gutter(spans: &[crate::layout::TextSpan]) -> Option<f32> {
        let body: Vec<&crate::layout::TextSpan> = spans
            .iter()
            .filter(|s| {
                !s.text.trim().is_empty()
                    && s.bbox.width > 0.0
                    && s.bbox.x.is_finite()
                    && s.bbox.width.is_finite()
            })
            .collect();
        if body.len() < 8 {
            return None;
        }
        let cmin = body.iter().map(|s| s.bbox.x).fold(f32::INFINITY, f32::min);
        let cmax = body
            .iter()
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        let content_w = cmax - cmin;
        if content_w < 100.0 {
            return None;
        }
        // Exactly one clean corridor near mid-page (bridge-excluded). 0 = single
        // column; ≥2 = grid/form/table.
        let mut boxes: Vec<(f32, f32)> = body
            .iter()
            .filter(|s| s.bbox.width <= 0.6 * content_w)
            .map(|s| (s.bbox.x, s.bbox.x + s.bbox.width))
            .collect();
        if boxes.len() < 8 {
            return None;
        }
        boxes.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
        let mut cover = boxes[0].1;
        let (mut corridors, mut gutter_x) = (0usize, 0.0f32);
        for &(l, r) in &boxes[1..] {
            if l - cover >= 12.0 {
                corridors += 1;
                gutter_x = (cover + l) * 0.5;
            }
            cover = cover.max(r);
        }
        if corridors != 1 || !(0.30..=0.70).contains(&((gutter_x - cmin) / content_w)) {
            return None;
        }
        // Column count via left-edge clustering. The coverage sweep above counts
        // *one* corridor whenever a single wide span in a column reaches past the
        // next column's start, hiding the real inter-column gap — so a two-page
        // spread (4 columns) collapses to its spread midline and reads as a clean
        // 2-column page, whose halves then each merge two real columns into an
        // interleaved row-major mess. Cluster the (non-full-width) span left edges
        // and require EXACTLY two significant column starts; anything else
        // (single column, 3+ columns, N-up spread) is rejected.
        {
            let mut lefts: Vec<f32> = boxes.iter().map(|&(l, _)| l).collect();
            lefts.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
            let clust_gap = 0.08 * content_w;
            let mut counts: Vec<usize> = Vec::new();
            let mut run = 1usize;
            let mut prev = lefts[0];
            for &v in &lefts[1..] {
                if v - prev > clust_gap {
                    counts.push(run);
                    run = 0;
                }
                run += 1;
                prev = v;
            }
            counts.push(run);
            // A "significant" column start carries ≥15% of the column-eligible
            // spans (a hanging-indent continuation merges into its start cluster
            // because its offset is far below clust_gap).
            let min_sig = (0.15 * lefts.len() as f32).ceil() as usize;
            let sig = counts.iter().filter(|&&c| c >= min_sig).count();
            if sig != 2 {
                return None;
            }
        }
        // Per-column region classification. The genuine discriminator between a
        // two-column PROSE/REFERENCE body (read column-major) and a table / form /
        // TOC that merely has one central gap (read row-wise) is the STRUCTURE of
        // each column, not a cross-gutter row-balance ratio. A cross-gutter
        // row-alignment gate measures alignment that ragged reference lists and
        // dense results columns do not have, so those were wrongly rejected and
        // fell to a row-major interleave. Classifying each half on its own
        // structure admits them while still rejecting tables/forms (which classify
        // as Table/Form). See `examples/classify_probe.rs`.
        let body_side = |want_left: bool| -> Vec<usize> {
            spans
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    !s.text.trim().is_empty()
                        && s.bbox.width > 0.0
                        && s.bbox.x.is_finite()
                        && s.bbox.width.is_finite()
                        && ((s.bbox.x + s.bbox.width * 0.5 < gutter_x) == want_left)
                })
                .map(|(i, _)| i)
                .collect()
        };
        let left_class = crate::layout::classify_region(spans, &body_side(true));
        let right_class = crate::layout::classify_region(spans, &body_side(false));
        if left_class.is_reorderable_column() && right_class.is_reorderable_column() {
            return Some(gutter_x);
        }
        // Fallback: the v0.3.66 cross-gutter content-balance test. The per-column
        // classifier (above) admits ragged reference lists / dense results columns
        // the balance test rejected, but it also REJECTS some genuine balanced
        // two-column PROSE the balance test accepted — short, ragged verse/body
        // lines on a narrow-gutter page (a reference-Bible / two-column page with a
        // full-width title, issue #734). Without this they fall off the
        // column-major path and the first row interleaves across the gutter. Tried
        // only AFTER the classifier (academic pages keep the classifier path) and
        // behind the same corridor + two-column-start preamble, so single-column /
        // grid / N-up pages never reach it.
        if Self::two_column_rows_balanced(spans, gutter_x) {
            return Some(gutter_x);
        }
        None
    }

    /// v0.3.66 cross-gutter content-balance test: true when spanning rows carry
    /// substantial text on BOTH sides of `gutter_x` (prose), not a short
    /// right-hand value / page number (form / TOC). Fallback for
    /// `prose_two_column_gutter` after the per-column classifier declines.
    fn two_column_rows_balanced(spans: &[crate::layout::TextSpan], gutter_x: f32) -> bool {
        let mut ordered: Vec<&crate::layout::TextSpan> = spans
            .iter()
            .filter(|s| {
                !s.text.trim().is_empty()
                    && s.bbox.width > 0.0
                    && s.bbox.x.is_finite()
                    && s.bbox.width.is_finite()
            })
            .collect();
        ordered.sort_by(|a, b| crate::utils::safe_float_cmp(b.bbox.y, a.bbox.y));
        let (mut total, mut spanning, mut short_r) = (0usize, 0usize, 0usize);
        let (mut lefts, mut rights): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i < ordered.len() {
            let y0 = ordered[i].bbox.y;
            let (mut lc, mut rc) = (0usize, 0usize);
            while i < ordered.len() && (ordered[i].bbox.y - y0).abs() <= 3.0 {
                let s = ordered[i];
                let n = s.text.trim().chars().count();
                if s.bbox.x + s.bbox.width * 0.5 < gutter_x {
                    lc += n;
                } else {
                    rc += n;
                }
                i += 1;
            }
            total += 1;
            if lc > 0 && rc > 0 {
                spanning += 1;
                lefts.push(lc);
                rights.push(rc);
                if rc < 15 {
                    short_r += 1;
                }
            }
        }
        if total < 6 || spanning == 0 || (spanning as f32) < 0.60 * total as f32 {
            return false;
        }
        if (short_r as f32) > 0.30 * spanning as f32 {
            return false;
        }
        let med = |v: &mut [usize]| -> f32 {
            v.sort_unstable();
            v[v.len() / 2] as f32
        };
        let (ml, mr) = (med(&mut lefts), med(&mut rights));
        mr >= 25.0 && (0.45..=2.2).contains(&(mr / ml.max(1.0)))
    }

    /// Robust classifier-gated two-column detector for bodies the clean corridor
    /// sweep (`prose_two_column_gutter`) and `is_multi_column_page`
    /// MISS — ragged reference lists and dense results columns. Their lines do
    /// not leave the single perfectly-clean empty corridor those detectors
    /// require (long entries occasionally bridge, ragged tails create extra
    /// gaps), so the page currently reads row-major (interleaved). This is the
    /// real-academic M1/M3 deficit.
    ///
    /// Strategy: find the emptiest vertical corridor in the central band, require
    /// it to be near-empty (a genuine gutter), require BALANCED + TALL columns on
    /// both sides (rejects single-column + margin note, and short side captions),
    /// and accept ONLY when both halves classify as reorderable (Prose/Reference)
    /// — so tables, forms, and single-column pages are rejected. Proven on the 5
    /// corpus discriminator PDFs (see `examples/classify_probe.rs`). Returns the
    /// gutter X on accept, else `None` (caller keeps prior behaviour).
    fn classifier_column_gutter(spans: &[crate::layout::TextSpan]) -> Option<f32> {
        let finite = |s: &crate::layout::TextSpan| {
            !s.text.trim().is_empty()
                && s.bbox.width > 0.0
                && s.bbox.x.is_finite()
                && s.bbox.width.is_finite()
                && s.bbox.y.is_finite()
        };
        let body: Vec<&crate::layout::TextSpan> = spans.iter().filter(|s| finite(s)).collect();
        if body.len() < 16 {
            return None;
        }
        let cmin = body.iter().map(|s| s.bbox.x).fold(f32::INFINITY, f32::min);
        let cmax = body
            .iter()
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        let content_w = cmax - cmin;
        if !content_w.is_finite() || content_w < 100.0 {
            return None;
        }
        let ymin = body.iter().map(|s| s.bbox.y).fold(f32::INFINITY, f32::min);
        let ymax = body
            .iter()
            .map(|s| s.bbox.y)
            .fold(f32::NEG_INFINITY, f32::max);
        let body_h = (ymax - ymin).max(1.0);

        // COLUMN-CONTENT spans = those NOT spanning most of the content width.
        // Full-width spans (titles, the abstract block, section headings, running
        // footers) are BANDS, excluded from gutter detection and classification:
        // counting them would (a) hide the corridor on a mixed page whose top is a
        // full-width title/abstract and bottom is two columns (every paper's
        // page 1), and (b) pollute the per-column class. The corridor, balance,
        // height, and class gates all operate on column-content spans;
        // `reorder_column_major_with_bands` re-emits the bands at their own Y.
        let band_w = 0.6 * content_w;
        let col_idx: Vec<usize> = (0..spans.len())
            .filter(|&i| finite(&spans[i]) && spans[i].bbox.width <= band_w)
            .collect();
        if col_idx.len() < 16 {
            return None;
        }

        // Scan the central band [0.30, 0.70] at fine resolution and find the
        // WIDEST near-empty vertical corridor — the real inter-column gutter —
        // then place the gutter at its midpoint. Picking the widest run (not just
        // any minimal-straddle point) is load-bearing for hanging-indent
        // reference columns: a ragged-ref page has TWO empty corridors — the true
        // gutter between the columns, and a narrow decoy between the right
        // column's hanging entry numbers and its indented text. The decoy is
        // narrower, so the widest-run rule lands the gutter correctly between the
        // columns (otherwise the entry numbers fall into the left column). A
        // single-column body has NO wide empty central corridor (its lines are
        // full-width → excluded above → too few column spans), so it returns None.
        let lo = cmin + 0.30 * content_w;
        let hi = cmin + 0.70 * content_w;
        let step = (content_w / 400.0).clamp(0.5, 3.0);
        // "Empty" tolerates a few stray straddlers (noise / a rare long token).
        let empty_max = (0.01 * col_idx.len() as f32).ceil() as usize;
        let straddle_at = |x: f32| -> usize {
            col_idx
                .iter()
                .filter(|&&i| {
                    spans[i].bbox.x + 2.0 < x && spans[i].bbox.x + spans[i].bbox.width - 2.0 > x
                })
                .count()
        };
        let (mut best_lo, mut best_hi) = (f32::NAN, f32::NAN);
        let (mut run_start, mut in_run, mut best_w) = (lo, false, 0.0f32);
        let mut x = lo;
        while x <= hi {
            if straddle_at(x) <= empty_max {
                if !in_run {
                    run_start = x;
                    in_run = true;
                }
            } else if in_run {
                let w = x - run_start;
                if w > best_w {
                    best_w = w;
                    best_lo = run_start;
                    best_hi = x;
                }
                in_run = false;
            }
            x += step;
        }
        if in_run {
            let w = hi - run_start;
            if w > best_w {
                best_w = w;
                best_lo = run_start;
                best_hi = hi;
            }
        }
        // Require a corridor of real width (a genuine gutter, not a glyph gap).
        if !best_lo.is_finite() || best_w < 6.0 {
            return None;
        }
        let gutter = (best_lo + best_hi) * 0.5;

        // Balanced, tall columns on both sides of the gutter (column-content only).
        let (mut left_idx, mut right_idx): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        let (mut ly0, mut ly1) = (f32::INFINITY, f32::NEG_INFINITY);
        let (mut ry0, mut ry1) = (f32::INFINITY, f32::NEG_INFINITY);
        for &i in &col_idx {
            let s = &spans[i];
            if s.bbox.x + s.bbox.width * 0.5 < gutter {
                left_idx.push(i);
                ly0 = ly0.min(s.bbox.y);
                ly1 = ly1.max(s.bbox.y);
            } else {
                right_idx.push(i);
                ry0 = ry0.min(s.bbox.y);
                ry1 = ry1.max(s.bbox.y);
            }
        }
        let nb = left_idx.len() + right_idx.len();
        if nb == 0 {
            return None;
        }
        // Each side carries a real share of the column content (rejects
        // 1 col + margin note).
        if (left_idx.len() as f32) < 0.30 * nb as f32 || (right_idx.len() as f32) < 0.30 * nb as f32
        {
            return None;
        }
        // Both columns must be tall and of comparable height — they sit BESIDE
        // each other. This rejects a short side-caption/figure-label beside a tall
        // body column, while allowing a mixed page where the columns occupy only
        // the lower portion below a full-width title/abstract (so the floor is
        // 0.4·body_h, not 0.5). `body_h` spans the whole page (title included).
        let (lext, rext) = (ly1 - ly0, ry1 - ry0);
        if lext < 0.4 * body_h || rext < 0.4 * body_h || lext.min(rext) < 0.5 * lext.max(rext) {
            return None;
        }
        // Class gate (load-bearing). NEITHER half may be Table/Form — that is the
        // hard table/form rejection (tables classify Table via mean_chars<10,
        // label/value pages classify Form). AND at least one half must be clearly
        // Prose/Reference, to anchor that this really is a text body. A `Mixed`
        // half is admitted alongside a Prose/Reference half: a dense results
        // column often classifies Mixed (figures, equations, and inline-citation
        // fragments lower its wide-line ratio below the Prose threshold), but it
        // is NOT a table (those are Table, not Mixed), so column-major reading is
        // still correct. Two Mixed halves (no clear prose anchor) stay rejected.
        use crate::layout::RegionClass;
        let lc = crate::layout::classify_region(spans, &left_idx);
        let rc = crate::layout::classify_region(spans, &right_idx);
        let is_table_or_form = |c| matches!(c, RegionClass::Table | RegionClass::Form);
        if is_table_or_form(lc) || is_table_or_form(rc) {
            return None;
        }
        if !(lc.is_reorderable_column() || rc.is_reorderable_column()) {
            return None;
        }
        Some(gutter)
    }

    /// B1: merge a contiguous same-line run of spans that *crosses the page
    /// midline* into a single span, so the converter pipeline's column cut
    /// cannot shred a full-width line that the producer happened to draw as two
    /// adjacent show-strings. A generator may emit a full-width heading line
    /// above a two-column body as two fragments split mid-word at the gutter;
    /// each fragment's centre-x then falls in a different column, so the
    /// geometric reading order buckets them apart and the second half surfaces
    /// far away in the other column's stream. The plain-text path never hits
    /// this because it runs `merge_adjacent_spans` *before* column detection;
    /// this mirrors that for the md/html converter paths.
    ///
    /// Returns `Some(new spans)` only when at least one gutter-crossing run was
    /// merged, else `None` (the caller keeps the original spans, byte-identical).
    /// Tightly scoped: fires only on a multi-column page, and only merges a run
    /// whose member spans are contiguous (small inter-span gap, same font size)
    /// AND whose combined x-extent straddles the content midline — the unique
    /// signature of a full-width line drawn as multiple fragments. A normal
    /// per-column body run never crosses the midline (the gutter gap breaks the
    /// contiguity at the column edge), so those pages are untouched.
    fn coalesce_gutter_crossing_runs(
        spans: &[crate::layout::TextSpan],
    ) -> Option<Vec<crate::layout::TextSpan>> {
        use crate::utils::safe_float_cmp;
        if spans.len() < 2 || !Self::is_multi_column_page(spans) {
            return None;
        }
        let finite = |s: &crate::layout::TextSpan| {
            s.bbox.x.is_finite() && s.bbox.y.is_finite() && s.bbox.width.is_finite()
        };
        let cmin = spans
            .iter()
            .filter(|s| finite(s) && !s.text.trim().is_empty())
            .map(|s| s.bbox.x)
            .fold(f32::INFINITY, f32::min);
        let cmax = spans
            .iter()
            .filter(|s| finite(s) && !s.text.trim().is_empty())
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        if !cmin.is_finite() || !cmax.is_finite() || cmax - cmin < 100.0 {
            return None;
        }
        let mid = (cmin + cmax) * 0.5;

        // Group original indices into visual lines by a font-relative Y band.
        let mut order: Vec<usize> = (0..spans.len()).filter(|&i| finite(&spans[i])).collect();
        order.sort_by(|&a, &b| {
            safe_float_cmp(spans[b].bbox.y, spans[a].bbox.y)
                .then_with(|| safe_float_cmp(spans[a].bbox.x, spans[b].bbox.x))
        });

        // index → action: a merged span replaces the run's first member; the
        // rest are dropped. Everything else is kept in its original position.
        let mut replacement: std::collections::HashMap<usize, crate::layout::TextSpan> =
            std::collections::HashMap::new();
        let mut dropped: std::collections::HashSet<usize> = std::collections::HashSet::new();

        let mut li = 0;
        while li < order.len() {
            let anchor_y = spans[order[li]].bbox.y;
            let mut lj = li + 1;
            while lj < order.len() {
                let fs = spans[order[li]]
                    .font_size
                    .max(spans[order[lj]].font_size)
                    .max(1.0);
                if (spans[order[lj]].bbox.y - anchor_y).abs() > 0.5 * fs {
                    break;
                }
                lj += 1;
            }
            // `order[li..lj]` is one visual line, already x-ascending.
            let line = &order[li..lj];
            let mut k = 0;
            while k < line.len() {
                // Extend a contiguous run: small inter-span gap + matching font size.
                let mut m = k + 1;
                while m < line.len() {
                    let prev = &spans[line[m - 1]];
                    let cur = &spans[line[m]];
                    let fs = prev.font_size.max(cur.font_size).max(1.0);
                    let gap = cur.bbox.x - (prev.bbox.x + prev.bbox.width);
                    // Keep a run UNIFORM in styling. `merge_same_line_run` takes
                    // the first span's style for the whole merged span, so a run
                    // that mixes weights/italics would silently erase an interior
                    // emphasis span (a lone bold word bracketed by body text loses
                    // its `**`). A genuine mid-word gutter split is always one
                    // word in one style, so requiring uniform weight+italic keeps
                    // that fix while never dropping emphasis.
                    if gap > 0.5 * fs
                        || (cur.font_size - prev.font_size).abs() > 0.1
                        || cur.font_weight != prev.font_weight
                        || cur.is_italic != prev.is_italic
                    {
                        break;
                    }
                    m += 1;
                }
                let run = &line[k..m];
                if run.len() >= 2 {
                    let run_min = spans[run[0]].bbox.x;
                    let last = &spans[run[run.len() - 1]];
                    let run_max = last.bbox.x + last.bbox.width;
                    // Only stitch a gutter-crossing run that actually carries a
                    // MID-WORD split straddling the midline: an adjacent pair
                    // joined by a hairline/negative gap whose two sides are both
                    // alphanumeric (the producer broke one word into two
                    // show-strings across the gutter). A legitimate full-width
                    // line drawn as several fragments breaks at WORD boundaries
                    // (space-width gaps) or non-letter glyph edges, so it never
                    // matches and stays byte-identical — this keeps the change
                    // scoped to the split-word case alone.
                    let has_midword_split_across_mid = run.windows(2).any(|w| {
                        let prev = &spans[w[0]];
                        let cur = &spans[w[1]];
                        let fs = prev.font_size.max(cur.font_size).max(1.0);
                        let gap = cur.bbox.x - (prev.bbox.x + prev.bbox.width);
                        let prev_tok = prev.text.split_whitespace().last().unwrap_or("");
                        let cur_tok = cur.text.split_whitespace().next().unwrap_or("");
                        // A genuine producer split breaks ONE word or number into two
                        // adjacent show-strings, leaving a tiny fragment on one side:
                        // a lone uppercase initial (a name abbreviation) or the two
                        // halves of a digit pair. On a tight multi-column page a font
                        // with over-estimated advance widths inflates each column
                        // line's bbox so a left-column line overruns the gutter and
                        // abuts the next column, making two DIFFERENT complete words
                        // look like a hairline split. Those join two complete tokens,
                        // so the split is real only when a boundary token is a lone
                        // uppercase letter or a two-digit number — never a 2-letter
                        // word, a complete word, or a bare figure/table digit. The gap
                        // can't separate the two cases (a genuine split overlaps as
                        // deeply as a column line), but a deep overlap (≥ half an em)
                        // is always a column abut.
                        let tok_is_fragment = |t: &str| {
                            let mut cs = t.chars();
                            match (cs.next(), cs.next(), cs.next()) {
                                // A lone uppercase letter — a name initial.
                                (Some(c), None, _) => c.is_alphabetic() && c.is_uppercase(),
                                // The two digits of a split number.
                                (Some(a), Some(b), None) => {
                                    a.is_ascii_digit() && b.is_ascii_digit()
                                }
                                _ => false,
                            }
                        };
                        gap >= -0.5 * fs
                            && gap <= 0.15 * fs
                            && prev.bbox.x < mid
                            && cur.bbox.x + cur.bbox.width > mid
                            && (tok_is_fragment(prev_tok) || tok_is_fragment(cur_tok))
                            && prev_tok
                                .chars()
                                .next_back()
                                .is_some_and(|c| c.is_alphanumeric())
                            && cur_tok.chars().next().is_some_and(|c| c.is_alphanumeric())
                    });
                    if run_min < mid && run_max > mid && has_midword_split_across_mid {
                        replacement.insert(run[0], Self::merge_same_line_run(spans, run));
                        for &idx in &run[1..] {
                            dropped.insert(idx);
                        }
                    }
                }
                k = m;
            }
            li = lj;
        }

        if replacement.is_empty() {
            return None;
        }
        // Rebuild in original order, substituting merged spans and dropping the
        // swallowed fragments; non-finite spans pass through untouched.
        let mut out = Vec::with_capacity(spans.len());
        for (i, s) in spans.iter().enumerate() {
            if dropped.contains(&i) {
                continue;
            }
            match replacement.remove(&i) {
                Some(merged) => out.push(merged),
                None => out.push(s.clone()),
            }
        }
        Some(out)
    }

    /// Merge a contiguous, x-ascending run of same-line spans (indices into
    /// `spans`) into one span: union the bounding box, take styling/MCID from the
    /// first member, and join the text with a single space wherever the
    /// inter-span gap is wide enough to be a word break. A negative / hairline
    /// gap is a mid-word fragment boundary and is concatenated directly, so the
    /// two halves of a word split across the gutter rejoin without a stray space.
    fn merge_same_line_run(
        spans: &[crate::layout::TextSpan],
        run: &[usize],
    ) -> crate::layout::TextSpan {
        let mut merged = spans[run[0]].clone();
        let mut x_max = merged.bbox.x + merged.bbox.width;
        for &idx in &run[1..] {
            let s = &spans[idx];
            let fs = merged.font_size.max(s.font_size).max(1.0);
            let gap = s.bbox.x - x_max;
            if gap > 0.25 * fs
                && !merged.text.ends_with(' ')
                && !s.text.starts_with(' ')
                && !merged.text.is_empty()
            {
                merged.text.push(' ');
            }
            merged.text.push_str(&s.text);
            x_max = x_max.max(s.bbox.x + s.bbox.width);
            merged.bbox.y = merged.bbox.y.min(s.bbox.y);
        }
        merged.bbox.width = (x_max - merged.bbox.x).max(0.0);
        merged.char_widths = Vec::new();
        merged
    }

    /// If `spans` is a genuine two-column-prose page (#734), reorder them
    /// column-major with full-width band separation; otherwise a no-op. Shared
    /// by `extract_text`, `to_markdown`, and `to_html` so every flow agrees on
    /// the reading order of a two-column body. Returns `true` if a reorder was
    /// applied (the caller can then suppress geometric block re-ordering that
    /// would otherwise re-derive row-major order from positions).
    fn reorder_two_column_prose(spans: &mut Vec<crate::layout::TextSpan>) -> bool {
        match Self::prose_two_column_gutter(spans) {
            Some(gutter_x) => {
                Self::reorder_column_major_with_bands(spans, gutter_x);
                true
            }
            None => false,
        }
    }

    /// Reorder a confirmed two-column-prose page **column-major with band
    /// separation** (#734). Walks rows top→bottom; a row containing a span that
    /// *crosses* the gutter is a full-width band (title, section heading,
    /// footer) and is emitted at its vertical position, between the column runs
    /// around it. Column runs are flushed left-column-then-right-column. This is
    /// the §14.8.3 layout model: full-width BLSEs interleave with columns by
    /// block position, so a mid-body heading is NOT split across the gutter.
    fn reorder_column_major_with_bands(spans: &mut Vec<crate::layout::TextSpan>, gutter_x: f32) {
        use crate::layout::TextSpan;
        // A genuine full-width BAND (title/heading/footer that spans both
        // columns) extends meaningfully on BOTH sides of the gutter. Require an
        // 8pt overhang each side so a column item whose bbox merely *clips* the
        // gutter by a few points — e.g. a hanging reference number ("42.") at the
        // right column's left edge, or a wrapped line reaching just past the
        // gutter — is NOT mistaken for a band and pulled out of its column.
        let crosses =
            |s: &TextSpan| s.bbox.x < gutter_x - 8.0 && s.bbox.x + s.bbox.width > gutter_x + 8.0;
        let mut src = std::mem::take(spans);
        // Top→bottom, then left→right within a row. Quantize Y to the row band
        // (`row_aware_span_cmp`) so sub-point baseline jitter between spans on
        // the SAME visual line (font-metric rounding, a superscript citation's
        // slightly different Y) cannot invert their X order: a 0.001pt Y
        // difference under a raw `safe_float_cmp` would sort a mid-line span
        // ahead of the line's left-edge span, scrambling the line
        // (PMC8129076 "phase and amplitude of clock-controlled genes83. Thus,
        // it is clear" — the "83" citation lifts ". Thus" onto a 0.001pt-higher
        // baseline). The downstream row grouping already uses a 3pt tolerance;
        // matching it here keeps the two consistent.
        src.sort_by(|a, b| {
            crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
        });
        let mut out: Vec<TextSpan> = Vec::with_capacity(src.len());
        let mut col_buf: Vec<TextSpan> = Vec::new();
        let flush = |buf: &mut Vec<TextSpan>, out: &mut Vec<TextSpan>| {
            if buf.is_empty() {
                return;
            }
            // A full line-height, used to decide when a block sits clearly
            // *below* the opposite column rather than beside it.
            let mut heights: Vec<f32> = buf.iter().map(|s| s.bbox.height).collect();
            heights.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
            let line_h = heights
                .get(heights.len() / 2)
                .copied()
                .unwrap_or(10.0)
                .max(1.0);
            // Left column (centre < gutter) by y, then right column by y.
            let (mut left, mut right): (Vec<TextSpan>, Vec<TextSpan>) = std::mem::take(buf)
                .into_iter()
                .partition(|s| s.bbox.x + s.bbox.width * 0.5 < gutter_x);
            // Row-banded (Y quantized to ROW_BAND_TOLERANCE_PT) so sub-point
            // baseline jitter on a single visual line cannot invert the X order
            // within that line; see the pre-sort note above.
            let by_yx = |a: &TextSpan, b: &TextSpan| {
                crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
            };
            // Trailing-block peel: a block lying a full line-height BELOW the
            // entire opposite column is a bottom-spanning block (e.g. a
            // bottom-left References section), not a parallel column member, so
            // it must read AFTER both columns at its own y — not within its
            // column partition (which would print it before the whole opposite
            // column). oxide bbox.y is top-up (higher y = higher on page), so
            // "below" = smaller y. Only fires when the opposite column has real
            // content (>=2 spans) and the block clears its bottom by a line, so
            // balanced 2-col bodies (columns ending at ~equal y) are untouched.
            let bottom_y =
                |v: &[TextSpan]| v.iter().map(|s| s.bbox.y).fold(f32::INFINITY, f32::min);
            let right_bottom = bottom_y(&right);
            let left_bottom = bottom_y(&left);
            let mut trailing: Vec<TextSpan> = Vec::new();
            if right.len() >= 2 {
                left.retain(|s| {
                    let below = s.bbox.y < right_bottom - line_h;
                    if below {
                        trailing.push(s.clone());
                    }
                    !below
                });
            }
            if left.len() >= 2 {
                right.retain(|s| {
                    let below = s.bbox.y < left_bottom - line_h;
                    if below {
                        trailing.push(s.clone());
                    }
                    !below
                });
            }
            left.sort_by(by_yx);
            right.sort_by(by_yx);
            trailing.sort_by(by_yx);
            out.append(&mut left);
            out.append(&mut right);
            out.append(&mut trailing);
        };
        let mut i = 0;
        while i < src.len() {
            let y0 = src[i].bbox.y;
            let mut row: Vec<TextSpan> = Vec::new();
            while i < src.len() && (src[i].bbox.y - y0).abs() <= 3.0 {
                row.push(src[i].clone());
                i += 1;
            }
            if row.iter().any(crosses) {
                // Full-width band row: flush the columns above it, then emit the
                // band whole (left→right), keeping it out of the column stream.
                flush(&mut col_buf, &mut out);
                row.sort_by(|a, b| crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x));
                out.append(&mut row);
            } else {
                col_buf.append(&mut row);
            }
        }
        flush(&mut col_buf, &mut out);
        *spans = out;
    }

    /// True when the page's multi-column geometric signal is explained by a
    /// detected TABLE rather than a genuine two-column text body.
    ///
    /// Used by the geometric reading-order dispatch: when the genuine
    /// two-column branches (topological / prose-gutter / classifier) all
    /// declined yet `is_multi_column_page` is still true, the page is either a
    /// single-column body with a data table (whose column-aligned cells trip the
    /// detector) or a two-column body the column branches missed. We only want
    /// to override the multi-column gate (and apply the row-aware band sort) in
    /// the FIRST case.
    ///
    /// Discriminator: cluster the per-line left edges of the spans OUTSIDE the
    /// detected table regions (the surrounding prose). A single-column body has
    /// ONE dominant left edge there; a genuine two-column body has two. We
    /// require a strong single dominant left-edge cluster (≥ 70% of non-table
    /// lines), so a two-column page — whose non-table prose still splits into two
    /// left-edge clusters — is rejected. Spans inside the table contribute their
    /// own column-aligned left edges and are deliberately excluded.
    fn multicol_signal_is_tabular(
        spans: &[crate::layout::TextSpan],
        tables: &[crate::structure::table_extractor::Table],
    ) -> bool {
        // Expand each table bbox slightly upward to absorb header rows the
        // spatial extractor often leaves just above the captured cell grid.
        let in_table = |s: &crate::layout::TextSpan| -> bool {
            let cx = s.bbox.x + s.bbox.width * 0.5;
            let cy = s.bbox.y + s.bbox.height * 0.5;
            tables.iter().any(|t| {
                t.bbox.is_some_and(|b| {
                    cx >= b.x - 2.0
                        && cx <= b.x + b.width + 2.0
                        && cy >= b.y - 2.0
                        && cy <= b.y + b.height + 14.0
                })
            })
        };
        let outside: Vec<&crate::layout::TextSpan> = spans
            .iter()
            .filter(|s| !s.text.trim().is_empty() && s.bbox.width > 0.0 && !in_table(s))
            .collect();
        if outside.len() < 8 {
            // The table is essentially the whole page; the row-aware band sort
            // linearises it correctly, so treat the signal as tabular.
            return true;
        }
        // Per-line (Y-band) minimum left edge.
        let mut by_band: std::collections::BTreeMap<i32, f32> = std::collections::BTreeMap::new();
        for s in &outside {
            let band = (s.bbox.y / 2.0).round() as i32;
            let e = by_band.entry(band).or_insert(f32::INFINITY);
            *e = e.min(s.bbox.x);
        }
        let mut lefts: Vec<f32> = by_band
            .values()
            .copied()
            .filter(|v| v.is_finite())
            .collect();
        if lefts.len() < 6 {
            return true;
        }
        lefts.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        // Cluster left edges with a 12pt gap (≈ one indent); the largest cluster
        // is the body's left margin.
        let mut clusters: Vec<usize> = Vec::new();
        let mut run = 1usize;
        let mut prev = lefts[0];
        for &v in &lefts[1..] {
            if v - prev > 12.0 {
                clusters.push(run);
                run = 0;
            }
            run += 1;
            prev = v;
        }
        clusters.push(run);
        let total = lefts.len();
        let top = *clusters.iter().max().unwrap_or(&0);
        // Strong single dominant left edge ⇒ single-column prose ⇒ the
        // multi-column signal came from the table.
        top as f32 >= 0.70 * total as f32
    }

    fn is_multi_column_page(spans: &[crate::layout::TextSpan]) -> bool {
        // Clean-gutter detector (handles short pages the histogram gates below
        // reject for lack of spans). A genuine empty vertical channel that no
        // span crosses, with multi-line content of overlapping vertical extent
        // on both sides, is the unambiguous geometric signature of side-by-side
        // columns — recoverable for untagged pages only from layout (XY-Cut,
        // ISO 32000-1 §9.4, since there is no logical-structure hint).
        if Self::has_clean_column_gutter(spans) {
            return true;
        }

        if spans.len() < 12 {
            return false; // too few to confidently split into columns
        }

        // Primary detector: line-start-X bimodality.
        //
        // The span-center histogram further down is noisy for word-level
        // spans (every X position has many word starts on multi-word
        // body-text lines). The reliable signal is the X position at
        // which each *line* begins — a two-column body has a strong
        // peak at the left-column-start X plus a strong peak at the
        // right-column-start X, with a clear empty gutter between
        // them. We cluster spans into lines by Y (1pt tolerance), pick
        // the leftmost X per line, and look for ≥ 2 peaks separated by
        // a gutter of ≥ 30pt with zero line-starts in it.
        if Self::has_bimodal_line_starts(spans) {
            return true;
        }

        let mut x_centers: Vec<f32> = spans
            .iter()
            .map(|s| s.bbox.x + s.bbox.width * 0.5)
            .collect();
        x_centers.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));

        // Degenerate CTM guard: drop centers more than MAX_EXTENT from the
        // median so a rogue span ~1e16 doesn't explode the histogram.
        const MAX_EXTENT_FROM_MEDIAN: f32 = 5_000.0;
        let median = x_centers[x_centers.len() / 2];
        x_centers.retain(|c| (*c - median).abs() <= MAX_EXTENT_FROM_MEDIAN);
        if x_centers.len() < 12 {
            return false;
        }

        let min = *x_centers.first().unwrap();
        let max = *x_centers.last().unwrap();
        let width = max - min;
        if width < 100.0 {
            return false; // spans cluster in a single vertical line — not columns
        }

        // Bin into 40 buckets; find peaks (≥ mean × 1.5) separated by at
        // least one empty bucket.
        const BUCKETS: usize = 40;
        let bucket_width = width / BUCKETS as f32;
        if bucket_width <= 0.0 {
            return false;
        }
        let mut hist = [0usize; BUCKETS];
        for c in &x_centers {
            let idx = (((c - min) / bucket_width) as usize).min(BUCKETS - 1);
            hist[idx] += 1;
        }

        let total: usize = hist.iter().sum();
        let mean = total as f32 / BUCKETS as f32;
        let threshold = (mean * 1.5).max(3.0);

        let mut peaks = 0usize;
        let mut in_peak = false;
        for &count in &hist {
            if count as f32 >= threshold {
                if !in_peak {
                    peaks += 1;
                    in_peak = true;
                }
            } else if count == 0 {
                in_peak = false;
            }
        }

        if peaks < 2 {
            return false;
        }

        // Confirmation: the peaks must have vertical overlap. If one "column"
        // is a footer and the other is the body, they don't interact — row-
        // aware is fine. Split spans into left-half vs right-half and check
        // their Y ranges overlap.
        let mid_x = (min + max) / 2.0;
        let mut left_y_min = f32::INFINITY;
        let mut left_y_max = f32::NEG_INFINITY;
        let mut right_y_min = f32::INFINITY;
        let mut right_y_max = f32::NEG_INFINITY;
        for s in spans {
            let cx = s.bbox.x + s.bbox.width * 0.5;
            if (cx - median).abs() > MAX_EXTENT_FROM_MEDIAN {
                continue;
            }
            let y_top = s.bbox.y + s.bbox.height;
            if cx < mid_x {
                left_y_min = left_y_min.min(s.bbox.y);
                left_y_max = left_y_max.max(y_top);
            } else {
                right_y_min = right_y_min.min(s.bbox.y);
                right_y_max = right_y_max.max(y_top);
            }
        }
        let left_span = (left_y_max - left_y_min).max(0.0);
        let right_span = (right_y_max - right_y_min).max(0.0);
        let overlap = left_y_max.min(right_y_max) - left_y_min.max(right_y_min);
        let min_span = left_span.min(right_span);
        if !(min_span > 0.0 && overlap > 0.5 * min_span) {
            return false;
        }

        // Require each half to contain enough spans to represent genuine body
        // text columns. Copyright pages, title pages, and other sparse layouts
        // can produce two X-center peaks with only 2–7 spans per "column" —
        // these are not true multi-column body text.
        let left_count = spans
            .iter()
            .filter(|s| {
                let cx = s.bbox.x + s.bbox.width * 0.5;
                (cx - median).abs() <= MAX_EXTENT_FROM_MEDIAN && cx < mid_x
            })
            .count();
        let right_count = spans.len() - left_count;
        if left_count.min(right_count) < 15 {
            return false;
        }

        // Font-aware column-shape gate.
        //
        // Real two-column body text has tight column-edge alignment:
        // most spans on each side share one dominant X position
        // (the column start), with a handful of indented or
        // section-header outliers. Scattered-fragment layouts spread
        // their spans evenly across many X positions on each side.
        //
        // Measure the fraction of side-spans that fall into the
        // largest X-cluster (cluster gap = `dominant_em`). Body text
        // typically scores ≥ 0.5; scattered layouts score < 0.4.
        // Reject pages where either side fails the threshold so
        // XY-cut doesn't mis-route scattered content as multi-column.
        let stats = crate::layout::PageFontStats::from_spans(spans);
        let cluster_gap = stats.dominant_em.max(4.0);
        let dominant_cluster_fraction = |take: &dyn Fn(f32) -> bool| -> f32 {
            let mut xs: Vec<f32> = spans
                .iter()
                .filter(|s| {
                    let cx = s.bbox.x + s.bbox.width * 0.5;
                    (cx - median).abs() <= MAX_EXTENT_FROM_MEDIAN && take(cx)
                })
                .map(|s| s.bbox.x)
                .collect();
            let total = xs.len();
            if total == 0 {
                return 0.0;
            }
            xs.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
            let mut best = 1usize;
            let mut current = 1usize;
            let mut last = xs[0];
            for &x in &xs[1..] {
                if x - last <= cluster_gap {
                    current += 1;
                    if current > best {
                        best = current;
                    }
                } else {
                    current = 1;
                }
                last = x;
            }
            best as f32 / total as f32
        };
        const MIN_DOMINANT_FRACTION: f32 = 0.5;
        let left_frac = dominant_cluster_fraction(&|cx| cx < mid_x);
        let right_frac = dominant_cluster_fraction(&|cx| cx >= mid_x);
        if left_frac >= MIN_DOMINANT_FRACTION && right_frac >= MIN_DOMINANT_FRACTION {
            return true;
        }

        // Additive accept path (no change to the gate above): shared-baseline
        // two-column bodies — academic references / bibliographies — read
        // left+right on the SAME Y line, so the row-aware sort interleaves
        // them. Their word-granular left edges scatter, so the dominant-
        // cluster gate above misses them. But they exhibit ONE persistent
        // vertical gutter corridor (the signal poppler/MuPDF use, independent
        // of line length). Detect it via within-line gap projection, prose-
        // guarded so numeric / short-cell tables — which also reach here —
        // stay on the row-aware path. See #607.
        Self::has_persistent_gutter_corridor(spans, median, MAX_EXTENT_FROM_MEDIAN)
    }

    /// Detect a single persistent vertical gutter corridor across the page —
    /// the geometric fingerprint of a two-column prose body whose columns
    /// share Y baselines (so `has_bimodal_line_starts` and the dominant-
    /// cluster gate both miss it). Mirrors `detect_narrow_gutter_prose`
    /// (`src/pipeline/reading_order/xycut.rs`) at the document-routing layer.
    ///
    /// Table-safe by construction (#536). Long-line bodies
    /// (`mean non-whitespace chars per line > 20`) keep the original
    /// concentration / coverage / centre accept path. Short-line bodies
    /// (verse / lexicon editions) are admitted only under stricter,
    /// length-independent guards a numeric / short-cell table cannot satisfy:
    /// higher concentration and coverage, left/right column char-mass balance,
    /// and a grid-row signal (a multi-cell table has ≥ 2 wide gaps on most
    /// rows; a two-column body has one gutter). Full-width display-math /
    /// heading rows are excluded from the gutter-coverage denominator so a
    /// minority of them does not veto an otherwise two-column page.
    fn has_persistent_gutter_corridor(
        spans: &[crate::layout::TextSpan],
        median: f32,
        max_extent: f32,
    ) -> bool {
        // Group spans into lines by rounded Y baseline; carry left/right
        // extents for gap projection and char count for the prose guard.
        let mut lines: std::collections::BTreeMap<i32, (Vec<(f32, f32)>, usize)> =
            std::collections::BTreeMap::new();
        let mut x_min = f32::MAX;
        let mut x_max = f32::MIN;
        for s in spans {
            let cx = s.bbox.x + s.bbox.width * 0.5;
            if (cx - median).abs() > max_extent {
                continue; // degenerate-CTM guard, same as the caller
            }
            let y_key = (s.bbox.y + s.bbox.height).round() as i32;
            let entry = lines.entry(y_key).or_default();
            entry.0.push((s.bbox.x, s.bbox.x + s.bbox.width));
            entry.1 += s.text.chars().filter(|c| !c.is_whitespace()).count();
            x_min = x_min.min(s.bbox.x);
            x_max = x_max.max(s.bbox.x + s.bbox.width);
        }
        let region_width = x_max - x_min;
        if lines.len() < 12 || region_width < 200.0 {
            return false;
        }

        let total_chars: usize = lines.values().map(|(_, c)| *c).sum();
        let mean_chars = total_chars as f32 / lines.len() as f32;

        // Largest within-line gap per line (≥ 6 pt suppresses word spacing);
        // record the gap midpoint X. Also flag full-width lines with no internal
        // gutter (display equations, full-width headings) so they neither support
        // nor veto the corridor — they are excluded from the coverage denominator
        // (Part 1b: display-math robustness, #536/arxiv_math).
        const MIN_GAP_PT: f32 = 6.0;
        let mut gap_positions: Vec<f32> = Vec::new();
        let mut full_width_lines = 0usize;
        let mut multi_gap_lines = 0usize;
        for (line_spans, _) in lines.values() {
            if line_spans.is_empty() {
                continue;
            }
            let mut sorted = line_spans.clone();
            sorted.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
            let line_left = sorted.first().map(|s| s.0).unwrap_or(0.0);
            let line_right = sorted.last().map(|s| s.1).unwrap_or(0.0);
            let mut largest_gap = 0.0_f32;
            let mut largest_mid = 0.0_f32;
            let mut significant_gaps = 0usize;
            for w in sorted.windows(2) {
                let gap = w[1].0 - w[0].1;
                if gap >= MIN_GAP_PT {
                    significant_gaps += 1;
                }
                if gap > largest_gap {
                    largest_gap = gap;
                    largest_mid = (w[0].1 + w[1].0) * 0.5;
                }
            }
            if (line_right - line_left) >= region_width * 0.9 && largest_gap < MIN_GAP_PT {
                full_width_lines += 1;
            }
            // A line with two or more wide internal gaps is a grid row (≥ 3
            // cells), not a two-column body line (one gutter). Used by the
            // short-line table discriminator below.
            if significant_gaps >= 2 {
                multi_gap_lines += 1;
            }
            if largest_gap >= MIN_GAP_PT {
                gap_positions.push(largest_mid);
            }
        }
        if gap_positions.len() < 12 {
            return false;
        }
        // Coverage denominator excludes full-width display rows.
        let eff_lines = lines.len().saturating_sub(full_width_lines).max(1);

        // Cluster gap midpoints (10 pt radius); find the dominant corridor.
        const CLUSTER_RADIUS_PT: f32 = 10.0;
        gap_positions.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        let mut best_size = 0usize;
        let mut best_center = 0.0_f32;
        let mut left = 0usize;
        let mut right = 0usize;
        let mut prefix: Vec<f32> = Vec::with_capacity(gap_positions.len() + 1);
        prefix.push(0.0);
        for &x in &gap_positions {
            prefix.push(prefix.last().unwrap() + x);
        }
        for &pivot in &gap_positions {
            while left < gap_positions.len() && gap_positions[left] < pivot - CLUSTER_RADIUS_PT {
                left += 1;
            }
            while right < gap_positions.len() && gap_positions[right] <= pivot + CLUSTER_RADIUS_PT {
                right += 1;
            }
            let count = right - left;
            if count > best_size {
                best_size = count;
                best_center = (prefix[right] - prefix[left]) / count as f32;
            }
        }

        // Gutter must sit near the page centre (0.30–0.70). A true two-column
        // body splits down the middle; a table's dominant gap (label column vs
        // data, or one of several cell boundaries) sits off-centre.
        let gutter_offset = best_center - x_min;
        let centre_ok =
            gutter_offset >= region_width * 0.30 && gutter_offset <= region_width * 0.70;
        if best_size < 16 || !centre_ok {
            return false;
        }

        if mean_chars > 20.0 {
            // Long-line two-column prose (the v0.3.57 accept path, unchanged
            // except the coverage denominator now excludes display rows):
            // concentration ≥ 62 %, coverage ≥ 50 % of (effective) lines.
            return best_size * 50 >= gap_positions.len() * 31 && best_size * 2 >= eff_lines;
        }

        // Short-line bodies (verse / lexicon / dictionary editions, #536): the
        // raw `mean_chars` floor used to reject these along with short-cell
        // tables. Admit them only under STRICTER, length-independent guards a
        // short-cell table cannot satisfy (Part 1a).
        let strict_concentration = best_size * 10 >= gap_positions.len() * 7; // ≥ 70 %
        let strict_coverage = best_size * 5 >= eff_lines * 3; // ≥ 60 % of lines
        if !(strict_concentration && strict_coverage) {
            return false;
        }
        // Column char-mass balance: each side of the gutter must carry ≥ 35 % of
        // the non-whitespace characters. A narrow label / verse-number column
        // paired with wide data is lopsided and rejected.
        let (mut left_chars, mut right_chars) = (0usize, 0usize);
        for s in spans {
            let cx = s.bbox.x + s.bbox.width * 0.5;
            if (cx - median).abs() > max_extent {
                continue;
            }
            let n = s.text.chars().filter(|c| !c.is_whitespace()).count();
            if cx < best_center {
                left_chars += n;
            } else {
                right_chars += n;
            }
        }
        let total = (left_chars + right_chars).max(1) as f32;
        if (left_chars as f32) < total * 0.35 || (right_chars as f32) < total * 0.35 {
            return false;
        }
        // Grid-row discriminator: a two-column body has ONE wide gap per line
        // (the gutter); a multi-cell numeric table has ≥ 2 wide gaps on most
        // rows (cell boundaries). Reject when the majority of lines are grid
        // rows — this is what keeps short-cell tables off the XY-cut path
        // without the raw `mean_chars` floor that also blocked short verse.
        multi_gap_lines * 2 <= eff_lines
    }

    /// RW-1: reading order for a **narrow-sidebar + wide-body** page (the MDPI /
    /// academic first-page layout: a full-width title band on top, a narrow left
    /// metadata column — Citation / Editor / Received / copyright — beside a wide
    /// body column). `is_multi_column_page` misreads this as two balanced columns
    /// and the XY-cut then slices the full-width title along the body gutter
    /// (§14.8.3: a block-level full-width element must NOT be column-assigned).
    ///
    /// Returns `Some(reordered_spans)` only when the layout is *confidently* this
    /// shape — emit order **band (title) + body, merged top→bottom, then the
    /// sidebar last** (the gold puts the title whole on top, then the body; the
    /// metadata sidebar is publisher furniture read last). Returns `None`
    /// otherwise so the normal XY-cut / row-aware path is unchanged. The gate is
    /// deliberately tight (gutter left-of-centre + a narrow left column + a wide
    /// right column + a full-width band near the top) so balanced two-column and
    /// single-column pages never reach it.
    fn sidebar_body_reading_order(
        spans: &[crate::layout::TextSpan],
    ) -> Option<Vec<crate::layout::TextSpan>> {
        use crate::utils::safe_float_cmp;
        if spans.len() < 30 {
            return None;
        }
        let x_min = spans.iter().map(|s| s.bbox.left()).fold(f32::MAX, f32::min);
        let x_max = spans
            .iter()
            .map(|s| s.bbox.right())
            .fold(f32::MIN, f32::max);
        let y_min = spans.iter().map(|s| s.bbox.top()).fold(f32::MAX, f32::min);
        let y_max = spans
            .iter()
            .map(|s| s.bbox.bottom())
            .fold(f32::MIN, f32::max);
        let width = (x_max - x_min).max(1.0);
        let height = (y_max - y_min).max(1.0);
        if !(width.is_finite() && height.is_finite()) {
            return None;
        }

        // Cluster spans into baseline lines (top→bottom).
        let mut order: Vec<usize> = (0..spans.len()).collect();
        order.sort_by(|&a, &b| {
            safe_float_cmp(spans[b].bbox.bottom(), spans[a].bbox.bottom())
                .then_with(|| safe_float_cmp(spans[a].bbox.left(), spans[b].bbox.left()))
        });
        struct Line {
            top_y: f32,
            min_left: f32,
            max_right: f32,
            members: Vec<usize>,
        }
        let mut lines: Vec<Line> = Vec::new();
        for &i in &order {
            let s = &spans[i];
            let h = (s.bbox.bottom() - s.bbox.top()).abs().max(1.0);
            match lines.last_mut() {
                Some(l) if (l.top_y - s.bbox.bottom()).abs() <= h * 0.6 => {
                    l.min_left = l.min_left.min(s.bbox.left());
                    l.max_right = l.max_right.max(s.bbox.right());
                    l.members.push(i);
                }
                _ => lines.push(Line {
                    top_y: s.bbox.bottom(),
                    min_left: s.bbox.left(),
                    max_right: s.bbox.right(),
                    members: vec![i],
                }),
            }
        }
        if lines.len() < 12 {
            return None;
        }

        // Find the body-column left edge: the most common line-start that sits
        // well right of the page left margin (excludes the sidebar/title cluster
        // anchored at x_min). The gutter is just left of it.
        let body_left = {
            const BIN: f32 = 5.0;
            let nbins = ((width / BIN).ceil() as usize).clamp(1, 4096);
            let mut hist = vec![0usize; nbins];
            for l in &lines {
                if l.min_left > x_min + width * 0.10 {
                    let b = (((l.min_left - x_min) / BIN) as usize).min(nbins - 1);
                    hist[b] += 1;
                }
            }
            let (peak, &cnt) = hist.iter().enumerate().max_by_key(|(_, &c)| c)?;
            if cnt < 5 {
                return None; // no consistent body-column start
            }
            x_min + peak as f32 * BIN
        };
        // Gutter must be left-of-centre (a narrow sidebar, not a centred 2-col).
        let gutter = body_left - width * 0.02;
        if !(gutter > x_min + width * 0.12 && gutter < x_min + width * 0.45) {
            return None;
        }

        // A full-width TITLE/heading row is typeset as many narrow word spans, so
        // no single span satisfies the per-span band test below; its leftmost words
        // (right edge ≤ gutter) would be miswept into the SIDEBAR and emitted last,
        // shattering the title (e.g. an MDPI first page where the title sits in a
        // large font across the full width, above the metadata sidebar + body).
        // Detect these per LINE: a baseline line whose member words FLOW
        // CONTINUOUSLY across the gutter (collective extent crosses it, ≥40% of the
        // page width, and no large internal gap straddling the gutter) is a true
        // full-width band — its words are evenly spaced, unlike a sidebar-label +
        // body-line that merely SHARE a baseline (e.g. "Accepted: 1 March 2021"
        // next to "1. Introduction"), which leaves a wide empty gutter corridor
        // between the two columns. Members of such band lines are forced into the
        // BAND group so the whole title row stays together at its vertical slot.
        // The straddling-gap gate keeps shared sidebar/body baselines split.
        let mut band_line_members: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        const MAX_STRADDLE_GAP_FRAC: f32 = 0.06; // ≈ gutter-corridor width
                                                 // The title/author band sits ABOVE the body column (the body starts at the
                                                 // affiliations/abstract). Restrict band promotion to lines above the
                                                 // topmost PURE-BODY line (a line whose words all begin at/right of the
                                                 // gutter, with no left-of-gutter member). Below that Y the page is the
                                                 // two-column sidebar+body region, where a wide crossing line is a
                                                 // sidebar-label + body-line sharing a baseline (e.g. "Switzerland." next to
                                                 // "cancer, atrial fibrillation…") whose tight gutter corridor would
                                                 // otherwise pass the straddle-gap gate and wrongly glue the sidebar inline.
                                                 // Larger bottom-y == higher on the page (PDF user space).
        let body_top_y = lines
            .iter()
            .filter(|l| l.min_left >= gutter)
            .map(|l| l.top_y)
            .fold(f32::NEG_INFINITY, f32::max);
        for line in &lines {
            if !(line.min_left < gutter
                && line.max_right > gutter
                && (line.max_right - line.min_left) > width * 0.40)
            {
                continue;
            }
            // Only the top-of-page title/author band, above the body column.
            if body_top_y.is_finite() && line.top_y < body_top_y {
                continue;
            }
            // Largest gap between consecutive members that straddles the gutter.
            let mut xs: Vec<(f32, f32)> = line
                .members
                .iter()
                .map(|&i| (spans[i].bbox.left(), spans[i].bbox.right()))
                .collect();
            xs.sort_by(|a, b| safe_float_cmp(a.0, b.0));
            let mut max_straddle_gap = 0.0f32;
            let mut prev_right = f32::NEG_INFINITY;
            for &(l, r) in &xs {
                if prev_right.is_finite() && prev_right < gutter && l > gutter {
                    max_straddle_gap = max_straddle_gap.max(l - prev_right);
                }
                prev_right = prev_right.max(r);
            }
            if max_straddle_gap < width * MAX_STRADDLE_GAP_FRAC {
                for &i in &line.members {
                    band_line_members.insert(i);
                }
            }
        }

        // Classify each SPAN by the gutter. A publisher-metadata sidebar and the
        // body usually SHARE baselines (the metadata column interleaves with body
        // lines by Y), so a per-line cluster would fuse them into one full-width
        // line and hide the sidebar — classify per span instead. BAND = a span
        // genuinely spanning the gutter (a wide full-width title/heading), or a
        // member of a continuous full-width band LINE (above). SIDEBAR = a span
        // entirely left of the gutter. BODY = everything at/right of it.
        let mut band: Vec<usize> = Vec::new();
        let mut sidebar: Vec<usize> = Vec::new();
        let mut body: Vec<usize> = Vec::new();
        for (i, s) in spans.iter().enumerate() {
            let l = s.bbox.left();
            let r = s.bbox.right();
            if band_line_members.contains(&i)
                || (l < gutter && r > gutter && (r - l) > width * 0.40)
            {
                band.push(i);
            } else if r <= gutter {
                sidebar.push(i);
            } else {
                body.push(i);
            }
        }
        // A real sidebar/body are each multi-line.
        let line_count = |v: &[usize]| -> usize {
            let mut ys: Vec<f32> = v.iter().map(|&i| spans[i].bbox.bottom()).collect();
            ys.sort_by(|a, b| safe_float_cmp(*a, *b));
            ys.dedup_by(|a, b| (*a - *b).abs() <= 2.0);
            ys.len()
        };
        if line_count(&sidebar) < 5 || line_count(&body) < 8 {
            return None;
        }
        // Sidebar genuinely narrower than the body column.
        let col_width = |v: &[usize]| -> f32 {
            let lo = v
                .iter()
                .map(|&i| spans[i].bbox.left())
                .fold(f32::MAX, f32::min);
            let hi = v
                .iter()
                .map(|&i| spans[i].bbox.right())
                .fold(f32::MIN, f32::max);
            (hi - lo).max(0.0)
        };
        let sw = col_width(&sidebar);
        let bw = col_width(&body);
        if sw >= width * 0.45 || sw >= bw * 0.70 {
            return None; // left column not a narrow sidebar relative to the body
        }
        // ANTI-FORM discriminator. A bare narrow left column is geometrically
        // indistinguishable from a label:value form (Name:/Address:/Date:) or a
        // verse/margin-note page, and these PDFs carry NO background tint to anchor
        // the sidebar. The reliable signal is semantic: a publisher-metadata
        // sidebar carries recognisable furniture labels that never head a form
        // field or a body column. Require >=2 DISTINCT labels so ordinary narrow
        // columns and forms never engage this reordering.
        let side_text: String = {
            let mut t = String::new();
            for &i in &sidebar {
                t.push_str(&spans[i].text.to_lowercase());
                t.push(' ');
            }
            t
        };
        const FURNITURE: [&str; 12] = [
            "citation",
            "received",
            "accepted",
            "published",
            "copyright",
            "licensee",
            "academic editor",
            "publisher",
            "doi.org",
            "issn",
            "creative commons",
            "open access",
        ];
        let furniture_hits = FURNITURE.iter().filter(|k| side_text.contains(**k)).count();
        if furniture_hits < 2 {
            return None;
        }

        // Emit: band + body merged top→bottom (title stays on top, body flows,
        // any mid-body full-width element keeps its vertical slot), then the
        // sidebar furniture last. Spans within a line read left→right.
        let mut main: Vec<usize> = band;
        main.extend(body);
        let key = |idx: &usize| {
            let s = &spans[*idx];
            (s.bbox.bottom(), s.bbox.left())
        };
        main.sort_by(|a, b| {
            let (ay, ax) = key(a);
            let (by, bx) = key(b);
            safe_float_cmp(by, ay).then_with(|| safe_float_cmp(ax, bx))
        });
        sidebar.sort_by(|a, b| {
            let (ay, ax) = key(a);
            let (by, bx) = key(b);
            safe_float_cmp(by, ay).then_with(|| safe_float_cmp(ax, bx))
        });
        let mut out: Vec<crate::layout::TextSpan> = Vec::with_capacity(spans.len());
        for i in main.into_iter().chain(sidebar) {
            out.push(spans[i].clone());
        }
        Some(out)
    }

    /// Order spans by a topological sort over text BLOCKS (a precede relation),
    /// for pages with genuine side-by-side regions (a two-column body, a
    /// two-column footer, a sidebar beside the body) that a flat row-aware (y,x)
    /// sort interleaves row-by-row (splicing one region's line into the other's).
    /// Returns `None` for any page WITHOUT two horizontally-disjoint,
    /// vertically-overlapping blocks (single-column and simple stacked layouts),
    /// so their output stays byte-identical.
    ///
    /// Coordinate convention (see `row_aware_span_cmp`): larger Y = higher on the
    /// page, read first; `bottom()` is a span's UPPER edge, `top()` its LOWER edge.
    fn topological_block_order(
        spans: &[crate::layout::TextSpan],
    ) -> Option<Vec<crate::layout::TextSpan>> {
        use crate::utils::safe_float_cmp;
        if spans.len() < 8 {
            return None;
        }
        let hi = |s: &crate::layout::TextSpan| s.bbox.bottom(); // upper edge (larger y)
        let lo = |s: &crate::layout::TextSpan| s.bbox.top(); // lower edge (smaller y)
        let med_h = {
            let mut hs: Vec<f32> = spans
                .iter()
                .map(|s| (hi(s) - lo(s)).abs())
                .filter(|h| h.is_finite() && *h > 0.0)
                .collect();
            if hs.is_empty() {
                return None;
            }
            hs.sort_by(|a, b| safe_float_cmp(*a, *b));
            hs[hs.len() / 2].max(1.0)
        };

        // Item 4 (M3): measure the page's single central column gutter (if any).
        // Used below to forbid a same-line union ACROSS the gutter regardless of
        // the measured gap: on dense two-column pages with tight leading, an
        // over-wide advance can make a cross-gutter gap < med_h, fusing the two
        // columns into one block so the side_by_side gate then declines and the
        // page falls to a row-major interleave. `None` (single-column /
        // multi-corridor / off-centre) ⇒ the predicate is byte-identical.
        let gutter_x = Self::measure_single_central_gutter(spans)
            .or_else(|| Self::density_central_gutter(spans));

        // --- Union-find: connect spans in the same text region. Two spans join
        // iff they are on the same line and horizontally adjacent (a normal word
        // gap, NOT a column gutter), OR vertically stacked with overlapping X and
        // a small inter-line gap. A column gutter (≥ ~1 em of whitespace) never
        // connects, so left/right columns become separate blocks even when their
        // lines share Y bands. ---
        let n = spans.len();
        let mut parent: Vec<usize> = (0..n).collect();
        fn find(parent: &mut [usize], mut x: usize) -> usize {
            while parent[x] != x {
                parent[x] = parent[parent[x]];
                x = parent[x];
            }
            x
        }
        // Index spans by reading order so each only tests a local window.
        let mut ord: Vec<usize> = (0..n).collect();
        ord.sort_by(|&a, &b| {
            safe_float_cmp(hi(&spans[b]), hi(&spans[a]))
                .then_with(|| safe_float_cmp(spans[a].bbox.left(), spans[b].bbox.left()))
        });
        let x_overlap = |a: &crate::layout::TextSpan, b: &crate::layout::TextSpan| -> f32 {
            (a.bbox.right().min(b.bbox.right()) - a.bbox.left().max(b.bbox.left())).max(0.0)
        };
        for (p, &i) in ord.iter().enumerate() {
            let si = &spans[i];
            // Look ahead over a bounded window of following spans in reading order.
            for &j in ord.iter().skip(p + 1).take(40) {
                let sj = &spans[j];
                let dy_centers = ((hi(si) + lo(si)) - (hi(sj) + lo(sj))).abs() * 0.5;
                let same_line = dy_centers < med_h * 0.5;
                let connect = if same_line {
                    // Horizontal neighbour: gap below ~1 em (word space), not a gutter.
                    let gap = (si.bbox.left().max(sj.bbox.left()))
                        - (si.bbox.right().min(sj.bbox.right()));
                    // Item 4 (M3): never join two spans that straddle the measured
                    // central gutter (one wholly left of it, the other wholly
                    // right), independent of `gap` — a tight-leading over-wide
                    // advance can otherwise make the cross-gutter gap < med_h and
                    // fuse the columns. Purely subtractive: it can only PREVENT a
                    // union, never create one, so `gutter_x == None` is byte-identical.
                    let crosses_gutter = gutter_x.is_some_and(|gx| {
                        (si.bbox.right() <= gx && sj.bbox.left() >= gx)
                            || (sj.bbox.right() <= gx && si.bbox.left() >= gx)
                    });
                    !crosses_gutter && gap < med_h * 1.0
                } else {
                    // Vertical neighbour: overlap in X and a small inter-line gap.
                    let vgap = (lo(si).min(lo(sj)) - hi(si).max(hi(sj))).abs();
                    // (distance between the nearer edges)
                    let near = (lo(si) - hi(sj)).abs().min((lo(sj) - hi(si)).abs());
                    x_overlap(si, sj) > med_h * 0.3 && near < med_h * 1.5 && vgap < med_h * 6.0
                };
                if connect {
                    let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                    if ri != rj {
                        parent[ri] = rj;
                    }
                }
            }
        }

        // --- Build blocks from the union-find components. A BTreeMap (not a
        // HashMap) keys the components by root index so `into_values()` is
        // DETERMINISTIC — HashMap iteration order is randomized per run, which
        // would make the block order (and thus the extracted text) flaky for
        // pages where two blocks tie on the seed sort key. ---
        use std::collections::BTreeMap;
        let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for i in 0..n {
            let r = find(&mut parent, i);
            groups.entry(r).or_default().push(i);
        }
        struct Block {
            x0: f32,
            x1: f32,
            y_hi: f32,
            y_lo: f32,
            members: Vec<usize>,
        }
        let blocks: Vec<Block> = groups
            .into_values()
            .map(|members| {
                let mut b = Block {
                    x0: f32::MAX,
                    x1: f32::MIN,
                    y_hi: f32::MIN,
                    y_lo: f32::MAX,
                    members,
                };
                for &i in &b.members {
                    let s = &spans[i];
                    b.x0 = b.x0.min(s.bbox.left());
                    b.x1 = b.x1.max(s.bbox.right());
                    b.y_hi = b.y_hi.max(hi(s));
                    b.y_lo = b.y_lo.min(lo(s));
                }
                b
            })
            .collect();
        if blocks.len() < 2 {
            return None;
        }

        // A STRUCTURED/TABULAR page (a chess-move table, a data grid, a TOC with a
        // page-number rail) shatters into many tiny fragment blocks the union-find
        // cannot coalesce — isolated cells, page numbers, single-letter column
        // heads — interleaved with the column runs. Flowing multi-column prose and
        // a sidebar+body do not: they are a handful of big blocks. So when fragment
        // blocks (< 4 spans) outnumber the substantial ones, the page is tabular
        // and must stay row-aware, NOT be read column-major.
        let fragments = blocks.iter().filter(|b| b.members.len() < 4).count();
        if fragments > blocks.len() - fragments {
            return None;
        }

        // Item 4 follow-up (M3): if the page has a clean central column gutter but
        // the union-find STILL produced a block that fuses the two columns across
        // it, the topological emit would interleave them (the block's spans get
        // sorted row-aware within the block). This happens on dense two-column
        // bodies where a producer-malformed full-width fragment, or a chain of
        // vertical unions through a wide line, bridges the columns despite the
        // same-line cross-gutter veto. A correct two-column block decomposition
        // has NO block straddling the gutter with substantial content on BOTH
        // sides. When one exists, bail to None so the dispatch falls through to
        // the band-aware column-major reader (`classifier_column_gutter` /
        // `reorder_column_major_with_bands`), which separates the columns and
        // re-emits full-width bands at their own Y. Gated on a measured gutter, so
        // pages without one (the common case) are byte-identical.
        if let Some(gx) = gutter_x {
            let fused = blocks.iter().any(|b| {
                if b.x0 >= gx - med_h || b.x1 <= gx + med_h {
                    return false; // does not straddle the gutter
                }
                // Count this block's members clearly on each side of the gutter.
                let mut l = 0usize;
                let mut r = 0usize;
                for &i in &b.members {
                    let s = &spans[i];
                    if s.bbox.right() <= gx {
                        l += 1;
                    } else if s.bbox.left() >= gx {
                        r += 1;
                    }
                }
                // Substantial content on BOTH sides ⇒ fused columns, not a band.
                l >= 4 && r >= 4
            });
            if fused {
                return None;
            }
        }

        // --- GATE: require ≥2 blocks that are horizontally DISJOINT yet overlap
        // in Y (genuine side-by-side regions). Single-column / stacked layouts
        // have none, so they return None and stay byte-identical. ---
        let y_ov = |a: &Block, b: &Block| (a.y_hi.min(b.y_hi) - a.y_lo.max(b.y_lo)) > med_h * 0.5;
        let x_disjoint = |a: &Block, b: &Block| a.x1 <= b.x0 || b.x1 <= a.x0;
        // Character density per block (≈ chars per text line). A page-number
        // column in a TOC, or the value column of a label:value form/table, is
        // text-SPARSE (a few chars per line); genuine prose columns and a
        // publisher metadata sidebar are text-DENSE. Used to reject row-paired
        // tables/TOCs/forms (which must read row-wise, NOT column-major).
        let char_density = |b: &Block| -> f32 {
            let mut ys: Vec<f32> = b.members.iter().map(|&i| hi(&spans[i])).collect();
            ys.sort_by(|p, q| safe_float_cmp(*p, *q));
            let mut lines = 1usize;
            for w in ys.windows(2) {
                if (w[1] - w[0]).abs() > med_h * 0.6 {
                    lines += 1;
                }
            }
            let chars: usize = b
                .members
                .iter()
                .map(|&i| spans[i].text.trim().chars().count())
                .sum();
            chars as f32 / lines as f32
        };
        // Number of baseline rows a block spans (same Y-clustering as char_density).
        let block_lines = |b: &Block| -> usize {
            let mut ys: Vec<f32> = b.members.iter().map(|&i| hi(&spans[i])).collect();
            ys.sort_by(|p, q| safe_float_cmp(*p, *q));
            let mut lines = 1usize;
            for w in ys.windows(2) {
                if (w[1] - w[0]).abs() > med_h * 0.6 {
                    lines += 1;
                }
            }
            lines
        };

        // Both side-by-side blocks must be SUBSTANTIAL, text-DENSE, multi-line
        // regions that overlap over several lines — a genuine 2-column body/footer
        // or a sidebar+body. Incidental overlaps (a drop cap, a page number, a
        // margin note, a fragmented poem line) involve tiny blocks or a sliver of
        // Y-overlap; row-paired tables/TOCs/forms have a text-sparse value column.
        // Neither must engage the reorder, or single-column poetry, decorated
        // pages, and TOCs scramble.
        let side_by_side = blocks.iter().enumerate().any(|(i, a)| {
            blocks.iter().skip(i + 1).any(|b| {
                x_disjoint(a, b)
                    && a.members.len() >= 8
                    && b.members.len() >= 8
                    && (a.y_hi.min(b.y_hi) - a.y_lo.max(b.y_lo)) > med_h * 3.0
                    // Each side must be a genuine MULTI-LINE column (≥ 4 rows). A
                    // single-column page whose body happens to end in just a
                    // couple of lines can have a wide intra-line word gap (a
                    // sentence space after a period) split those lines into two
                    // x-disjoint blocks that otherwise pass this gate and emit as
                    // fake columns (alice_old "Looking-Glass House" p.226). A real
                    // two-column body / sidebar spans many rows.
                    && block_lines(a) >= 4
                    && block_lines(b) >= 4
                    && char_density(a) >= 12.0
                    && char_density(b) >= 12.0
                    // The two side-by-side blocks must be the page's DOMINANT
                    // content (≥ half the spans). A genuine 2-column body or
                    // sidebar+body lives in two big blocks; a table / chess
                    // diagram / dense diagram fragments into many small blocks
                    // that the union-find cannot coalesce, so the dominant pair
                    // never reaches half — leaving such pages on the row-aware path.
                    && (a.members.len() + b.members.len()) * 2 >= n
            })
        });
        if !side_by_side {
            return None;
        }

        // --- Topological order (two precede rules). A precedes B if they
        // overlap in X and A is above B (vertical stack), OR A is left of B and
        // they overlap in Y (side-by-side columns: left first). DFS with a visited
        // guard appends a block only after all its predecessors, and terminates on
        // any rule cycle. ---
        let nb = blocks.len();
        let before = |a: &Block, b: &Block| -> bool {
            let x_ov = (a.x1.min(b.x1) - a.x0.max(b.x0)) > med_h * 0.3;
            if x_ov && a.y_hi > b.y_hi && a.y_lo > b.y_lo {
                return true; // A stacked above B
            }
            if a.x1 <= b.x0 && y_ov(a, b) {
                return true; // A is the left column of a side-by-side pair
            }
            false
        };
        // Kahn's algorithm over the `before` relation. The previous
        // iterative DFS re-pushed every unvisited predecessor each time a
        // node was expanded (no on-stack marking), which is exponential in
        // stack growth on block graphs with heavy fan-in — a dense
        // equation page produced tens of gigabytes of stack and an OOM
        // kill. Kahn's is O(V^2) for the edge scan and O(V+E) after,
        // visits each block exactly once, and terminates unconditionally;
        // ready blocks are drained in reading order (top-left first) for
        // a stable result, matching the old seed order.
        let mut result_blocks: Vec<usize> = Vec::with_capacity(nb);
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); nb];
        let mut indegree: Vec<usize> = vec![0; nb];
        for a in 0..nb {
            for b in 0..nb {
                if a != b && before(&blocks[a], &blocks[b]) {
                    // a must come before b.
                    preds[a].push(b);
                    indegree[b] += 1;
                }
            }
        }
        let seed_order = |a: usize, b: usize| {
            safe_float_cmp(blocks[b].y_hi, blocks[a].y_hi)
                .then_with(|| safe_float_cmp(blocks[a].x0, blocks[b].x0))
        };
        // Kept sorted in REVERSE reading order so pop() takes the
        // top-left-most ready block.
        let mut ready: Vec<usize> = (0..nb).filter(|&i| indegree[i] == 0).collect();
        ready.sort_by(|&a, &b| seed_order(b, a));
        let mut emitted = vec![false; nb];
        while let Some(bi) = ready.pop() {
            // `ready` is kept sorted with the NEXT block last (reverse
            // reading order), so pop() takes the top-left-most.
            if emitted[bi] {
                continue;
            }
            emitted[bi] = true;
            result_blocks.push(bi);
            let mut newly_ready = false;
            for &succ in &preds[bi] {
                indegree[succ] -= 1;
                if indegree[succ] == 0 {
                    ready.push(succ);
                    newly_ready = true;
                }
            }
            if newly_ready {
                ready.sort_by(|&a, &b| seed_order(b, a));
            }
        }
        // The `before` relation is acyclic by construction (edges strictly
        // decrease y within a band or strictly increase x across columns),
        // but guard against float pathologies leaving blocks unemitted:
        // append any remainder in reading order rather than dropping text.
        if result_blocks.len() < nb {
            let mut rest: Vec<usize> = (0..nb).filter(|&i| !emitted[i]).collect();
            rest.sort_by(|&a, &b| seed_order(a, b));
            result_blocks.extend(rest);
        }

        // --- Emit: each block's spans in reading order (y desc, x asc). ---
        let mut out: Vec<crate::layout::TextSpan> = Vec::with_capacity(n);
        for &bi in &result_blocks {
            let mut members = blocks[bi].members.clone();
            members.sort_by(|&a, &b| {
                safe_float_cmp(hi(&spans[b]), hi(&spans[a]))
                    .then_with(|| safe_float_cmp(spans[a].bbox.left(), spans[b].bbox.left()))
            });
            for i in members {
                out.push(spans[i].clone());
            }
        }
        if out.len() == n {
            Some(out)
        } else {
            None
        }
    }

    /// True if the spans cluster into lines whose leftmost X positions
    /// form ≥ 2 distinct peaks separated by a clear gutter.
    ///
    /// Body-level word spans fill the X axis continuously, so the
    /// span-center histogram cannot tell two-column body text apart
    /// from a single-column page with varied line lengths. The line-
    /// start histogram does: in two-column body text most lines start
    /// at one of two X positions (left-column-start or right-column-
    /// start), and the wide gutter between the columns produces a
    /// long zero-count stretch.
    fn has_bimodal_line_starts(spans: &[crate::layout::TextSpan]) -> bool {
        const Y_BAND: f32 = 2.0;
        const BIN_PT: f32 = 5.0;
        const MIN_PEAK_COUNT: usize = 4;
        const MIN_GUTTER_PT: f32 = 30.0;

        if spans.len() < 24 {
            return false;
        }

        // Cluster spans into lines by Y (descending so top-of-page first).
        let mut lines: Vec<(f32, f32)> = Vec::new(); // (y, line_x_min)
        let mut sorted = spans.to_vec();
        sorted.sort_by(|a, b| {
            crate::utils::safe_float_cmp(b.bbox.y, a.bbox.y)
                .then_with(|| crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x))
        });

        let mut current_y: Option<f32> = None;
        let mut current_xmin: f32 = f32::INFINITY;
        for s in &sorted {
            match current_y {
                Some(y) if (y - s.bbox.y).abs() <= Y_BAND => {
                    current_xmin = current_xmin.min(s.bbox.x);
                }
                _ => {
                    if let Some(y) = current_y {
                        if current_xmin.is_finite() {
                            lines.push((y, current_xmin));
                        }
                    }
                    current_y = Some(s.bbox.y);
                    current_xmin = s.bbox.x;
                }
            }
        }
        if let Some(y) = current_y {
            if current_xmin.is_finite() {
                lines.push((y, current_xmin));
            }
        }
        if lines.len() < 16 {
            return false;
        }

        // Bin line-start X positions.
        let xmin = lines.iter().map(|(_, x)| *x).fold(f32::INFINITY, f32::min);
        let xmax = lines
            .iter()
            .map(|(_, x)| *x)
            .fold(f32::NEG_INFINITY, f32::max);
        if !(xmin.is_finite() && xmax.is_finite()) || xmax - xmin < MIN_GUTTER_PT {
            return false;
        }
        let bin_count = (((xmax - xmin) / BIN_PT).ceil() as usize).max(1);
        if bin_count > 4096 {
            return false; // degenerate CTM
        }
        let mut hist = vec![0usize; bin_count];
        for (_, x) in &lines {
            let idx = (((x - xmin) / BIN_PT) as usize).min(bin_count - 1);
            hist[idx] += 1;
        }

        // Scan for ≥ 2 peaks (count ≥ MIN_PEAK_COUNT) with a long
        // zero-count run between them.
        let mut peaks: Vec<usize> = Vec::new(); // bin indices (peak center)
        let mut in_peak = false;
        let mut peak_start = 0usize;
        for (i, &c) in hist.iter().enumerate() {
            if c >= MIN_PEAK_COUNT {
                if !in_peak {
                    peak_start = i;
                    in_peak = true;
                }
            } else if c == 0 && in_peak {
                peaks.push((peak_start + i.saturating_sub(1)) / 2);
                in_peak = false;
            }
        }
        if in_peak {
            peaks.push((peak_start + hist.len() - 1) / 2);
        }
        if peaks.len() < 2 {
            return false;
        }

        // Check gutter: at least one pair of consecutive peaks must
        // have ≥ MIN_GUTTER_PT zero-count between them.
        let gutter_bins = (MIN_GUTTER_PT / BIN_PT) as usize;
        for w in peaks.windows(2) {
            let a = w[0];
            let b = w[1];
            if b <= a {
                continue;
            }
            let zeros = hist[a + 1..b].iter().filter(|&&c| c == 0).count();
            if zeros >= gutter_bins {
                return true;
            }
        }
        false
    }

    /// Numeric value 0–9 of a folio (page-number) digit, or `None` if `c` is
    /// not one. Scoped to the decimal-digit blocks that actually appear as
    /// page folios: ASCII, Arabic-Indic, Extended Arabic-Indic (Persian/Urdu),
    /// Devanagari, and full-width. Deliberately narrower than
    /// `char::is_numeric()` (which also matches `½`, `①`, superscripts) and
    /// wider than `char::is_ascii_digit()`. CJK ideographic numerals
    /// (`一二三…`) are intentionally excluded — they are not Unicode `Nd`, and
    /// collapsing them would over-normalize real headings (`第一章` → `第#章`).
    ///
    /// `char::to_digit(10)` cannot stand in here: it is ASCII-only and returns
    /// `None` for `'٥'` / `'५'` / `'５'`, so each block is mapped to its zero
    /// code point directly.
    fn folio_digit_value(c: char) -> Option<u32> {
        let cp = c as u32;
        let base = match cp {
            0x0030..=0x0039 => 0x0030, // ASCII 0-9
            0x0660..=0x0669 => 0x0660, // Arabic-Indic
            0x06F0..=0x06F9 => 0x06F0, // Extended Arabic-Indic (Persian/Urdu)
            0x0966..=0x096F => 0x0966, // Devanagari
            0xFF10..=0xFF19 => 0xFF10, // Full-width
            _ => return None,
        };
        Some(cp - base)
    }

    /// Unicode-aware decimal-digit predicate for page folios. See
    /// [`Self::folio_digit_value`] for the supported blocks and rationale.
    fn is_folio_digit(c: char) -> bool {
        Self::folio_digit_value(c).is_some()
    }

    /// Normalize a span's text for cross-page signature matching.
    /// Collapses whitespace and replaces digit runs with `#` so that page
    /// numbers ("Page 1 of 10", "Page 2 of 10") collapse to one signature.
    /// Non-Latin folio digits (Arabic-Indic, Persian, Devanagari, full-width)
    /// collapse too, so folios paginated in those scripts share one signature.
    fn normalize_artifact_signature(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut in_digit_run = false;
        let mut last_was_space = true;
        for c in text.chars() {
            if Self::is_folio_digit(c) {
                if !in_digit_run {
                    out.push('#');
                    in_digit_run = true;
                }
                last_was_space = false;
            } else if c.is_whitespace() {
                if !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
                in_digit_run = false;
            } else {
                out.push(c);
                last_was_space = false;
                in_digit_run = false;
            }
        }
        out.trim().to_string()
    }

    /// Item 6B (M5): does a running-band literal look like a CONSTANT-text
    /// pagination / citation string — a DOI, a journal volume/issue/article
    /// reference, or a journal URL host — accompanied by a digit? Such strings
    /// recur identically on every page (so the varying-literal gate never catches
    /// them) yet are furniture that leaks into the body. The gate is deliberately
    /// narrow: it requires a recognised citation/URL token AND a digit, so a
    /// repeated facility name, document title, or ordinary sentence is NEVER
    /// matched (miss-rather-than-drop — a false positive deletes real content).
    fn looks_like_stable_pagination(literal: &str) -> bool {
        let l = literal.to_ascii_lowercase();
        // The digit gate is script-aware (a non-Latin folio digit still
        // qualifies); the citation/URL keyword tokens below remain English-
        // only by design — keyword universality is tracked separately.
        if !l.chars().any(Self::is_folio_digit) {
            return false;
        }
        if l.contains("doi.org") || l.contains("doi:") || l.contains("/doi/") {
            return true;
        }
        // Journal volume/issue/article reference. NB: "no." is deliberately
        // EXCLUDED — it also matches government-form control numbers like
        // "OMB No. 1545-0115", which are form content, not running furniture.
        if ["volume", "vol.", "article", "issue"]
            .iter()
            .any(|kw| l.contains(kw))
        {
            return true;
        }
        // Journal URL host in a running footer, e.g. "www.frontiersin.org 1".
        l.contains("www.") && (l.contains(".org") || l.contains(".com") || l.contains(".net"))
    }

    /// A page is treated as vertical-writing (CJK tategaki, 縦書き) when a
    /// majority of its non-empty text spans were rendered in WMode 1. The
    /// writing mode comes from the PDF's own `/WMode` (captured on each span
    /// via `GraphicsState::text_wmode`), so this is authoritative — a
    /// horizontal page (WMode 0) is never misclassified, and its
    /// running-header/footer detection is unchanged.
    fn page_is_vertical(spans: &[crate::layout::TextSpan]) -> bool {
        let mut vertical = 0usize;
        let mut total = 0usize;
        for s in spans {
            if s.text.trim().is_empty() {
                continue;
            }
            total += 1;
            if s.wmode == 1 {
                vertical += 1;
            }
        }
        total > 0 && vertical * 2 > total
    }

    /// Is `bbox` inside the candidate running-header/footer band for a page of
    /// the given dimensions? Horizontal pages use the top/bottom 12% strips.
    /// Vertical-writing (tategaki) pages *additionally* use the left/right 12%
    /// strips — the outer edge where CJK vertical folios and running heads
    /// conventionally sit, rather than across the top/bottom edge. The side
    /// strips are additive (the top/bottom test still applies), so this only
    /// ever widens detection, never narrows it.
    fn in_chrome_band(
        bbox: &crate::geometry::Rect,
        page_width: f32,
        page_height: f32,
        vertical: bool,
    ) -> bool {
        let vband = page_height * 0.12;
        if bbox.y < vband || bbox.y + bbox.height > page_height - vband {
            return true;
        }
        if vertical {
            let hband = page_width * 0.12;
            if bbox.x < hband || bbox.x + bbox.width > page_width - hband {
                return true;
            }
        }
        false
    }

    /// Ensure running-artifact signatures are computed (once) and return a
    /// clone for matching. The computation scans every page's raw spans,
    /// collects normalized text that appears in the top/bottom 12% band (and,
    /// on vertical-writing pages, the left/right 12% band), and keeps entries
    /// that recur on >=50% of pages.
    /// Article threads for this document, parsed once and shared.
    /// [`crate::structure::parse_article_threads`] walks the entire page tree,
    /// and reading-order resolution asks for them on every page.
    pub(crate) fn cached_article_threads(
        &self,
    ) -> std::sync::Arc<Vec<crate::structure::ArticleThread>> {
        if let Some(cached) = self.article_threads_cache.lock_or_recover().as_ref() {
            return std::sync::Arc::clone(cached);
        }
        let threads = std::sync::Arc::new(crate::structure::parse_article_threads(self));
        *self.article_threads_cache.lock_or_recover() = Some(std::sync::Arc::clone(&threads));
        threads
    }

    fn ensure_running_artifact_signatures(
        &self,
    ) -> Result<std::sync::Arc<std::collections::HashMap<String, usize>>> {
        {
            let guard = self.running_artifact_signatures.lock_or_recover();
            if let Some(ref map) = *guard {
                // Shared by reference: this runs once per page and the map is
                // document-wide, so cloning it here scaled with page count.
                return Ok(std::sync::Arc::clone(map));
            }
        }
        let page_count = self.page_count()?;
        if page_count < 2 {
            let empty = std::sync::Arc::new(std::collections::HashMap::new());
            *self.running_artifact_signatures.lock_or_recover() =
                Some(std::sync::Arc::clone(&empty));
            return Ok(empty);
        }

        // (count of distinct pages seeing the signature, first page it appeared on).
        // `first_seen_any` tracks the earliest page a signature appeared on
        // regardless of body-content — so if the cover page is all-chrome
        // (no body text), it still registers as "first seen" and gets its
        // title kept by the per-page mark_running_artifact_spans exemption.
        let mut occurrences: std::collections::HashMap<String, (usize, usize)> =
            std::collections::HashMap::new();
        let mut first_seen_any: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        // Track distinct literal texts per signature. A signature whose digits
        // are stable across every page (i.e. the literal text never changes) is
        // NOT a page-number-containing header — it is substantive content that
        // happens to repeat. Only suppress signatures where the literal text
        // varies (at least two distinct forms) meaning digits change per page.
        let mut literal_variants: std::collections::HashMap<
            String,
            std::collections::HashSet<String>,
        > = std::collections::HashMap::new();
        for pi in 0..page_count {
            let spans = match self.extract_spans_raw(pi) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let (page_width, page_height) = match self.get_page_media_box(pi) {
                Ok((_, _, w, h)) if h > 0.0 => (w, h),
                _ => continue,
            };
            let vertical = Self::page_is_vertical(&spans);
            // Require that the page has CONTENT outside the chrome band(s)
            // before counting band spans as candidate artifacts. Otherwise, a
            // page consisting only of a title near the top would have its own
            // title classified as a "running header" across all pages. (For
            // horizontal pages this is identical to the prior top/bottom test.)
            let has_body_content = spans.iter().any(|s| {
                !s.text.trim().is_empty()
                    && !Self::in_chrome_band(&s.bbox, page_width, page_height, vertical)
            });
            // Collect per-page unique signatures from the chrome bands.
            // Runs even when there's no body content so `first_seen_any`
            // registers the cover page even if it's all-chrome.
            let mut seen_this_page: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for s in spans.iter() {
                let trimmed = s.text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !Self::in_chrome_band(&s.bbox, page_width, page_height, vertical) {
                    continue;
                }
                let sig = Self::normalize_artifact_signature(trimmed);
                if sig.is_empty() || sig.chars().count() < 2 {
                    continue;
                }
                seen_this_page
                    .entry(sig)
                    .or_insert_with(|| trimmed.to_string());
            }
            // Track first-seen across ALL pages (even body-content-skipped)
            for sig in seen_this_page.keys() {
                first_seen_any.entry(sig.clone()).or_insert(pi);
            }
            // Track literal variants — if the literal text for a signature
            // differs across pages, the digits are varying (page numbers).
            for (sig, literal) in &seen_this_page {
                literal_variants
                    .entry(sig.clone())
                    .or_default()
                    .insert(literal.clone());
            }
            if !has_body_content {
                continue;
            }
            // Count only pages with body content for the recurrence threshold
            for sig in seen_this_page.into_keys() {
                let entry = occurrences.entry(sig).or_insert((0, pi));
                entry.0 += 1;
                if pi < entry.1 {
                    entry.1 = pi;
                }
            }
        }
        let threshold = (page_count as f32 * 0.5).ceil() as usize;
        let signatures: std::collections::HashMap<String, usize> = occurrences
            .into_iter()
            .filter(|(sig, (count, _))| {
                let variants = literal_variants.get(sig).map(|s| s.len()).unwrap_or(0);
                // Varying-literal path (page numbers / dates): the digits change per
                // page. Recurs on >=50% of body pages.
                if *count >= threshold.max(2) && variants >= 2 {
                    return true;
                }
                // Item 6B (M5): CONSTANT-literal pagination/citation (DOI, volume/
                // article, journal URL + digit). The literal never changes, so the
                // varying-literal gate above misses it. Require a STRICTER >=60%
                // recurrence AND the narrow citation/URL shape gate, so substantive
                // repeated content (facility names, titles) is never suppressed.
                let strict = (page_count as f32 * 0.6).ceil() as usize;
                if *count >= strict.max(2)
                    && variants < 2
                    && literal_variants
                        .get(sig)
                        .and_then(|s| s.iter().next())
                        .is_some_and(|lit| Self::looks_like_stable_pagination(lit))
                {
                    return true;
                }
                false
            })
            .map(|(sig, _)| {
                // Use the earliest page the signature appeared on — which
                // may be a body-content-skipped cover page that `occurrences`
                // didn't count toward the threshold but `first_seen_any` did.
                let first = first_seen_any.get(&sig).copied().unwrap_or(0);
                (sig, first)
            })
            .collect();
        let signatures = std::sync::Arc::new(signatures);
        *self.running_artifact_signatures.lock_or_recover() =
            Some(std::sync::Arc::clone(&signatures));
        Ok(signatures)
    }

    /// Mark spans near the top/bottom of the page whose normalized text
    /// matches a cached running-artifact signature by setting
    /// `artifact_type` to Pagination.
    /// #553: a bare page number (e.g. " 1 ", "12") varies per page, so it
    /// never matches a repeated-text signature and leaks into the body. Treat
    /// a short pure-digit token (1..=9999) as a page-number candidate — only
    /// applied inside the top/bottom margin band by the caller, so ordinary
    /// numerals in body text are never affected.
    fn is_bare_page_number_text(trimmed: &str) -> bool {
        // Bound by character count, not byte length: non-Latin folio digits are
        // 2–3 UTF-8 bytes each, so a byte cap would reject "۱۲۳" outright.
        if trimmed.is_empty() || trimmed.chars().count() > 4 {
            return false;
        }
        // Fold the (script-aware) digits to a value directly; `parse::<u32>`
        // and `char::to_digit` are ASCII-only and reject non-Latin folios.
        let mut value: u32 = 0;
        for c in trimmed.chars() {
            match Self::folio_digit_value(c) {
                Some(d) => value = value * 10 + d,
                None => return false,
            }
        }
        (1..=9999).contains(&value)
    }

    fn mark_running_artifact_spans(
        &self,
        page_index: usize,
        spans: &mut [crate::layout::TextSpan],
    ) -> Result<()> {
        let (_, _, page_width, page_height) = match self.get_page_media_box(page_index) {
            Ok(mb) => mb,
            Err(_) => return Ok(()),
        };
        if page_height <= 0.0 {
            return Ok(());
        }
        let vertical = Self::page_is_vertical(spans);
        // Snapshot baselines of every non-blank span, so the bare-page-number
        // rule can require a candidate to stand ALONE on its line (#553): a
        // digit adjacent to other text — e.g. the "8" in "8th" — is content,
        // not a page number.
        let occupied_baselines: Vec<f32> = spans
            .iter()
            .filter(|s| !s.text.trim().is_empty())
            .map(|s| s.bbox.y)
            .collect();
        // Signature set may be empty (no repeated headers/footers); the
        // bare-page-number rule below still runs.
        let signatures = self.ensure_running_artifact_signatures()?;
        for s in spans.iter_mut() {
            if s.artifact_type.is_some() {
                continue;
            }
            if !Self::in_chrome_band(&s.bbox, page_width, page_height, vertical) {
                continue;
            }
            let trimmed = s.text.trim();
            if trimmed.is_empty() {
                continue;
            }
            // #553: standalone page-number chrome in the margin band — only
            // when the digit is ISOLATED on its line (no other text span
            // within ~one line height), so digits embedded in words/runs are
            // never dropped.
            if Self::is_bare_page_number_text(trimmed) {
                let line_tol = s.font_size.max(6.0);
                let on_line = occupied_baselines
                    .iter()
                    .filter(|&&oy| (oy - s.bbox.y).abs() < line_tol)
                    .count();
                if on_line <= 1 {
                    s.artifact_type = Some(crate::extractors::text::ArtifactType::Pagination(
                        crate::extractors::text::PaginationSubtype::PageNumber,
                    ));
                }
                continue;
            }
            if signatures.is_empty() {
                continue;
            }
            let sig = Self::normalize_artifact_signature(trimmed);
            if let Some(&first_seen_on) = signatures.get(&sig) {
                // Keep the first appearance — it's usually the document
                // cover-page title that got classified as chrome only
                // because later pages repeat it as a running header (B3).
                if page_index == first_seen_on {
                    continue;
                }
                s.artifact_type = Some(crate::extractors::text::ArtifactType::Pagination(
                    crate::extractors::text::PaginationSubtype::Other,
                ));
            }
        }
        Ok(())
    }

    /// Internal helper: extract raw (unsorted) text spans from a page.
    ///
    /// This is the common extraction logic shared by `extract_spans`
    /// `extract_spans_with_reading_order`. Spans are returned without any
    /// sorting or erase-region filtering applied.
    fn extract_spans_raw(&self, page_index: usize) -> Result<Vec<crate::layout::TextSpan>> {
        self.extract_spans_raw_with_extraction_config(
            page_index,
            crate::extractors::TextExtractionConfig::default(),
        )
    }

    /// Internal helper: extract raw text spans using a specific extraction config.
    ///
    /// This allows callers to provide a [`TextExtractionConfig`] (optionally
    /// configured with an [`ExtractionProfile`]) to control TJ offset thresholds
    /// and word boundary detection during span extraction.
    fn extract_spans_raw_with_extraction_config(
        &self,
        page_index: usize,
        config: crate::extractors::TextExtractionConfig,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        self.extract_spans_impl(page_index, config, HashSet::new(), HashSet::new())
    }

    fn extract_spans_raw_filtered(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        self.extract_spans_impl(
            page_index,
            crate::extractors::TextExtractionConfig::default(),
            excluded_layers,
            excluded_inks,
        )
    }

    fn extract_spans_impl(
        &self,
        page_index: usize,
        config: crate::extractors::TextExtractionConfig,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        if self.is_encrypted_unreadable() {
            log::warn!("PDF is encrypted and could not be decrypted; returning no spans");
            return Ok(Vec::new());
        }
        use crate::extractors::TextExtractor;

        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        if self.page_cannot_have_text(page_dict) {
            return Ok(Vec::new());
        }

        let content_data = match self.get_page_content_data(page_index) {
            Ok(data) => data,
            Err(e) => {
                log::warn!(
                    "Failed to decode content stream for page {}: {}, returning empty",
                    page_index,
                    e
                );
                return Ok(Vec::new());
            }
        };

        if !Self::may_contain_text(&content_data) {
            return Ok(Vec::new());
        }

        let mut extractor = TextExtractor::with_config(config);
        // Stamp the page index so spans carry McidScope::Page(page_index)
        // by default; Form XObject Do invocations push their own scope
        // on top of the stack inside the extractor.
        extractor.set_page_index(page_index as u32);
        if !excluded_layers.is_empty() {
            extractor.set_excluded_layers(excluded_layers);
        }
        if !excluded_inks.is_empty() {
            extractor.set_excluded_inks(excluded_inks);
        }
        if let Some(resources) = page_dict.get("Resources") {
            extractor.set_resources(resources.clone());
            extractor.set_document(self);
            if let Err(e) = self.load_fonts(resources, &mut extractor) {
                log::warn!(
                    "Failed to load fonts for page {}: {}, continuing with defaults",
                    page_index,
                    e
                );
            }
        }

        let spans = extractor.extract_text_spans(&content_data)?;
        // Drain MCIDs whose in-stream /ActualText was applied during
        // extraction and stash on the document so the struct-tree-
        // scope applier honours MC-scope-wins precedence (§14.9.4).
        //
        // The per-page entry is REPLACED, not extended: every
        // `extract_spans_impl` call is a self-contained per-page
        // extraction and its own MC-scope detections must be
        // authoritative. Accumulating would make stale results from
        // an earlier filter-set leak into a later, differently-
        // filtered call.
        let mc_set = extractor.take_mc_actualtext_mcids();
        let mut guard = self.mc_actualtext_mcids.lock_or_recover();
        if mc_set.is_empty() {
            guard.remove(&page_index);
        } else {
            guard.insert(page_index, mc_set);
        }
        Ok(spans)
    }

    /// Extract text from a page, excluding content from specified layers and inks.
    ///
    /// Uses the same full text assembly pipeline as [`extract_text`](Self::extract_text)
    /// (structure-tree ordering, table detection, column detection), but with
    /// layer/ink-excluded spans removed before assembly.
    ///
    /// **Ink filtering note:** For DeviceN color spaces, text is suppressed if
    /// ANY ink in the DeviceN array matches an excluded ink name. Tint values
    /// are not evaluated — this is an all-or-nothing match.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `excluded_layers` - OCG layer names to suppress (empty = no layer filtering)
    /// * `excluded_inks` - Separation/DeviceN ink names to suppress (empty = no ink filtering)
    pub fn extract_text_filtered(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<String> {
        if excluded_layers.is_empty() && excluded_inks.is_empty() {
            return self.extract_text(page_index);
        }

        let spans = self.extract_spans_filtered(page_index, excluded_layers, excluded_inks)?;
        let options = crate::converters::ConversionOptions {
            extract_tables: true,
            ..Default::default()
        };
        self.assemble_text_from_spans(page_index, spans, &options)
    }

    /// Extract text from a region of a page with layer/ink filtering applied.
    ///
    /// Composes [`Self::extract_text_filtered`] with [`Self::extract_text_in_rect`]: spans
    /// are filtered by layer/ink first, then by region, then assembled via
    /// the full text pipeline (structure-tree ordering, table detection,
    /// column detection, whitespace + line breaks).
    pub fn extract_text_filtered_in_rect(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<String> {
        let spans = if excluded_layers.is_empty() && excluded_inks.is_empty() {
            self.extract_spans(page_index)?
        } else {
            self.extract_spans_filtered(page_index, excluded_layers, excluded_inks)?
        };
        let options = crate::converters::ConversionOptions {
            extract_tables: true,
            include_region: Some((region, mode)),
            ..Default::default()
        };
        self.assemble_text_from_spans(page_index, spans, &options)
    }

    /// Geometric `ColumnAware` (XY-cut) span ordering. Shared by the
    /// `ColumnAware` and `Structure` reading-order branches (the latter uses it
    /// as its baseline and its tiebreak for unstructured spans).
    fn order_spans_column_aware(
        &self,
        spans: Vec<crate::layout::TextSpan>,
        page_index: usize,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        use crate::pipeline::reading_order::{
            ReadingOrderContext as ROContext, ReadingOrderStrategy, XYCutStrategy,
        };
        let strategy = XYCutStrategy::new();
        let context = ROContext::new().with_page(page_index as u32);
        let ordered = strategy.apply(spans, &context)?;
        Ok(ordered.into_iter().map(|o| o.span).collect())
    }

    /// Extract text spans from a page using a specified reading order strategy.
    ///
    /// This method extracts text spans identically to [`extract_spans`](Self::extract_spans),
    /// then applies the chosen reading order strategy to sort them.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `reading_order` - The reading order strategy to apply
    ///
    /// # Returns
    ///
    /// Vector of TextSpan objects sorted according to the chosen reading order.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::{PdfDocument, ReadingOrder};
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("two_column.pdf")?;
    /// let spans = doc.extract_spans_with_reading_order(0, ReadingOrder::ColumnAware)?;
    /// for span in spans {
    ///     println!("{}", span.text);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_spans_with_reading_order(
        &self,
        page_index: usize,
        reading_order: ReadingOrder,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        self.extract_spans_filtered_with_reading_order(
            page_index,
            reading_order,
            HashSet::new(),
            HashSet::new(),
        )
    }

    /// Extract positioned spans in a reading order, excluding optional-content
    /// layers and/or Separation/DeviceN inks.
    ///
    /// This is [`extract_spans_with_reading_order`](Self::extract_spans_with_reading_order)
    /// and [`extract_text_filtered`](Self::extract_text_filtered) combined: the
    /// former cannot filter, and the latter returns assembled text rather than
    /// positioned spans. A consumer that lays spans out itself - an HTML/XML
    /// emitter placing each span at its own rectangle - needs both at once.
    ///
    /// The motivating case is render/extract parity. `render_page` honours the
    /// document's default configuration `/OCProperties/D`, but span extraction
    /// treats everything as visible unless the caller names layers (see the
    /// `optional_content` module note). Passing
    /// `optional_content::compute_default_off_ocgs(&doc)` as `excluded_layers`
    /// makes extraction agree with what the page actually displays - without it,
    /// a default-OFF layer holding a copy of the page contributes a SECOND copy
    /// of every word.
    ///
    /// Empty sets are exactly equivalent to the unfiltered call, so this is a
    /// superset of the existing API and costs nothing when no filtering is asked
    /// for.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `reading_order` - The reading order strategy to apply
    /// * `excluded_layers` - OCG layer names to suppress (empty = no filtering)
    /// * `excluded_inks` - Separation/DeviceN ink names to suppress (empty = none)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::{PdfDocument, ReadingOrder};
    /// # use pdf_oxide::optional_content;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let doc = PdfDocument::open("layered.pdf")?;
    /// // Agree with what the page displays: drop the default-off layers.
    /// let hidden = optional_content::compute_default_off_ocgs(&doc);
    /// let spans = doc.extract_spans_filtered_with_reading_order(
    ///     0,
    ///     ReadingOrder::ColumnAware,
    ///     hidden,
    ///     Default::default(),
    /// )?;
    /// for span in spans {
    ///     println!("{}", span.text);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_spans_filtered_with_reading_order(
        &self,
        page_index: usize,
        reading_order: ReadingOrder,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        // Extract raw spans using the common extraction logic. The unfiltered
        // path is kept verbatim for empty sets so existing callers are unchanged.
        let mut spans = if excluded_layers.is_empty() && excluded_inks.is_empty() {
            self.extract_spans_raw(page_index)?
        } else {
            self.extract_spans_raw_filtered(page_index, excluded_layers, excluded_inks)?
        };

        // Drop text lying entirely off the MediaBox - a doc that reuses one big
        // Form XObject across pages relies on the `W n` clip to hide the off-page
        // portion, which the raw extractor does not honour. `extract_spans` applies
        // this via `postprocess_spans`; the reading-order path must too, or it
        // emits every page's worth of spans (measured: a stats report emitted a
        // chart's full hidden data table, ~5x the visible label count).
        self.drop_offpage_spans(page_index, &mut spans);

        // Apply reading order strategy
        match reading_order {
            ReadingOrder::TopToBottom => {
                // Row-aware sort: Y-band descending, then X ascending.
                spans.sort_by(|a, b| {
                    crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
                });
            }
            ReadingOrder::ColumnAware => {
                spans = self.order_spans_column_aware(spans, page_index)?;
            }
            ReadingOrder::Structure => {
                // Geometric order is the baseline. The structure tree then fixes ONLY
                // the spans it can fix unambiguously: TABLE cells.
                //
                // A geometric XY-cut reads a wide table column-major and drops cells;
                // the structure tree's pre-order traversal gives the authoritative
                // row-major order (§14.8.2.3). But applying that traversal to the WHOLE
                // page also reorders flowing prose - where the tree's section order can
                // legitimately differ from visual order (it de-prioritises page
                // artifacts, for one) - which is a change, not an improvement. So table
                // content is reordered IN PLACE by structure rank while every non-table
                // span keeps its geometric position.
                let mut ordered = self.order_spans_column_aware(spans, page_index)?;
                if let Some(tree) = self.struct_tree_trustworthy() {
                    // Populate the structure-content cache, then read it (it carries the
                    // per-MCID `in_table` flag the mcid-order list does not).
                    let _ = self.cached_mcid_order_for_page(&tree, page_index as u32);
                    let content: Vec<crate::structure::OrderedContent> = self
                        .structure_content_cache
                        .lock_or_recover()
                        .as_ref()
                        .and_then(|c| c.get(&(page_index as u32)))
                        .cloned()
                        .unwrap_or_default();

                    let page_scope = crate::structure::McidScope::Page(page_index as u32);
                    // Structure rank for TABLE MCIDs only.
                    let mut table_rank: HashMap<(crate::structure::McidScope, u32), usize> =
                        HashMap::new();
                    for (i, c) in content.iter().enumerate() {
                        if let (true, Some(m)) = (c.in_table, c.mcid) {
                            let scope = c.mcid_scope.clone().unwrap_or_else(|| page_scope.clone());
                            table_rank.entry((scope, m)).or_insert(i);
                        }
                    }

                    if !table_rank.is_empty() {
                        let key_of = |s: &crate::layout::TextSpan| {
                            s.mcid.and_then(|m| {
                                let scope =
                                    s.mcid_scope.clone().unwrap_or_else(|| page_scope.clone());
                                table_rank
                                    .get(&(scope, m))
                                    .or_else(|| table_rank.get(&(page_scope.clone(), m)))
                                    .copied()
                            })
                        };
                        // The slots the table spans occupy in geometric order, and the
                        // table spans themselves.
                        let mut slots: Vec<usize> = Vec::new();
                        let mut cells: Vec<(usize, crate::layout::TextSpan)> = Vec::new();
                        for (idx, s) in ordered.iter().enumerate() {
                            if let Some(r) = key_of(s) {
                                slots.push(idx);
                                cells.push((r, s.clone()));
                            }
                        }
                        // Re-fill those exact slots with the cells in structure
                        // (row-major) order. Non-table spans never move.
                        cells.sort_by_key(|(r, _)| *r);
                        for (slot, (_, cell)) in slots.into_iter().zip(cells) {
                            ordered[slot] = cell;
                        }
                    }
                }
                spans = ordered;
            }
        }

        // Apply struct-tree-scope /ActualText (ISO 32000-1 §14.9.4).
        self.apply_actualtext_to_spans(page_index, &mut spans);

        Ok(spans)
    }

    /// Extract complete page text data in a single call.
    ///
    /// Returns a [`PageText`](crate::layout::text_block::PageText) containing spans in reading order, per-character
    /// data derived from those spans (using font-metric widths when available),
    /// and the page dimensions. Uses the default `TopToBottom` reading order.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("example.pdf")?;
    /// let page_text = doc.extract_page_text(0)?;
    /// println!("Page {}x{} pt", page_text.page_width, page_text.page_height);
    /// println!("{} spans, {} chars", page_text.spans.len(), page_text.chars.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_page_text(&self, page_index: usize) -> Result<crate::layout::PageText> {
        self.extract_page_text_with_options(page_index, ReadingOrder::default())
    }

    /// Extract complete page text data with a specific reading order.
    ///
    /// Like [`extract_page_text`](Self::extract_page_text) but allows choosing
    /// between `TopToBottom` and `ColumnAware` reading order.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `reading_order` - Reading order strategy to apply
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::{PdfDocument, ReadingOrder};
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("two_column.pdf")?;
    /// let page_text = doc.extract_page_text_with_options(0, ReadingOrder::ColumnAware)?;
    /// for span in &page_text.spans {
    ///     println!("{}", span.text);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_page_text_with_options(
        &self,
        page_index: usize,
        reading_order: ReadingOrder,
    ) -> Result<crate::layout::PageText> {
        // Get spans with the requested reading order
        let spans = self.extract_spans_with_reading_order(page_index, reading_order)?;

        // Derive chars from spans (uses char_widths for accurate positioning)
        let chars: Vec<crate::layout::TextChar> = spans.iter().flat_map(|s| s.to_chars()).collect();

        // Get page dimensions from MediaBox
        let media_box = self.get_page_media_box(page_index)?;

        Ok(crate::layout::PageText {
            spans,
            chars,
            page_width: media_box.2,
            page_height: media_box.3,
        })
    }

    /// Extract text spans from a page with custom configuration.
    ///
    /// This method allows controlling span merging behavior through configuration,
    /// including adaptive threshold settings for improved extraction quality.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `config` - SpanMergingConfig controlling extraction parameters
    ///
    /// # Returns
    ///
    /// A vector of TextSpan objects extracted from the page with applied configuration.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # use pdf_oxide::extractors::SpanMergingConfig;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("example.pdf")?;
    ///
    /// // Use adaptive threshold configuration
    /// let config = SpanMergingConfig::adaptive();
    /// let spans = doc.extract_spans_with_config(0, config)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_spans_with_config(
        &self,
        page_index: usize,
        config: crate::extractors::SpanMergingConfig,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        use crate::extractors::TextExtractor;

        // Get page object
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        // Fast pre-check: skip image-only pages before decompression
        if self.page_cannot_have_text(page_dict) {
            return Ok(Vec::new());
        }

        // Get content stream data — skip page on decode failure (Annex I)
        let content_data = match self.get_page_content_data(page_index) {
            Ok(data) => data,
            Err(e) => {
                log::warn!(
                    "Failed to decode content stream for page {}: {}, returning empty",
                    page_index,
                    e
                );
                return Ok(Vec::new());
            }
        };

        // Early-out for pages with no text content (§9.4.3)
        if !Self::may_contain_text(&content_data) {
            return Ok(Vec::new());
        }

        // Create text extractor with merged configuration
        let mut extractor = TextExtractor::new().with_merging_config(config);

        // Load fonts from page resources and set resources for XObject access
        if let Some(resources) = page_dict.get("Resources") {
            extractor.set_resources(resources.clone());
            extractor.set_document(self);

            // Load fonts
            if let Err(e) = self.load_fonts(resources, &mut extractor) {
                log::warn!(
                    "Failed to load fonts for page {}: {}, continuing with defaults",
                    page_index,
                    e
                );
            }
        }

        // Extract text spans
        extractor.extract_text_spans(&content_data)
    }

    /// Extract individual characters from a PDF page.
    ///
    /// This is a **low-level API** for character-level granularity. For most use cases,
    /// prefer `extract_spans()` which provides complete text strings as PDF defines them.
    ///
    /// # Character-level extraction details:
    ///
    /// - Returns individual `TextChar` objects with position, font, and style information
    /// - Characters are sorted in reading order (top-to-bottom, left-to-right)
    /// - Overlapping characters (rendered multiple times for effects) are deduplicated
    /// - Useful for layout analysis, debugging, or custom text processing pipelines
    ///
    /// # Arguments
    ///
    /// * `page_index` - Page number (0-indexed)
    ///
    /// # Returns
    ///
    /// Vector of `TextChar` objects in reading order, or error if extraction fails
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("document.pdf")?;
    /// let chars = doc.extract_chars(0)?;
    /// for ch in chars {
    ///     println!("'{}' at ({:.1}, {:.1}), font: {}",
    ///         ch.char, ch.bbox.x, ch.bbox.y, ch.font_name);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// List all Optional Content Group (OCG) layer names in the document.
    ///
    /// Reads `/OCProperties` from the document catalog and returns the `/Name`
    /// of each OCG dictionary listed in `/OCGs`. These names can be passed to
    /// `extract_text_filtered` / `extract_chars_filtered` via `excluded_layers`.
    ///
    /// Returns an empty vec if the document has no optional content.
    pub fn get_layers(&self) -> Result<Vec<String>> {
        let catalog = self.catalog()?;
        let catalog_dict = catalog
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf("Catalog is not a dictionary".to_string()))?;

        let oc_props = match catalog_dict.get("OCProperties") {
            Some(obj) => {
                if let Some(r) = obj.as_reference() {
                    self.load_object(r)?
                } else {
                    obj.clone()
                }
            }
            None => return Ok(Vec::new()),
        };

        let oc_dict = match oc_props.as_dict() {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };

        let ocgs_obj = match oc_dict.get("OCGs") {
            Some(obj) => {
                if let Some(r) = obj.as_reference() {
                    self.load_object(r)?
                } else {
                    obj.clone()
                }
            }
            None => return Ok(Vec::new()),
        };

        let ocgs_arr = match ocgs_obj.as_array() {
            Some(a) => a,
            None => return Ok(Vec::new()),
        };

        let mut names = Vec::new();
        for item in ocgs_arr {
            let ocg_obj = if let Some(r) = item.as_reference() {
                match self.load_object(r) {
                    Ok(o) => o,
                    Err(_) => continue,
                }
            } else {
                item.clone()
            };
            if let Some(d) = ocg_obj.as_dict() {
                if let Some(Object::Name(n)) = d.get("Name") {
                    names.push(n.clone());
                } else if let Some(Object::String(s)) = d.get("Name") {
                    if let Ok(text) = String::from_utf8(s.clone()) {
                        names.push(text);
                    }
                }
            }
        }
        Ok(names)
    }

    /// List ink / separation names used on a specific page.
    ///
    /// Scans the page's `/Resources /ColorSpace` dictionary for `/Separation`
    /// and `/DeviceN` color space definitions and returns their ink names.
    /// These names can be passed to `extract_text_filtered` /
    /// `extract_chars_filtered` via `excluded_inks`.
    ///
    /// **Note:** Only the page's own `/Resources` is walked. Spot inks
    /// declared inside a Form XObject's local `/Resources /ColorSpace`
    /// dictionary will not be enumerated — even though the renderer and
    /// extractor will still honor them at use time. Callers populating a
    /// UI picker from this list may miss XObject-local inks.
    ///
    /// For the full walk that follows `Do` operators into Form XObject
    /// resources, use [`Self::get_page_inks_deep`] — that is what the
    /// separation renderer uses to allocate plates.
    pub fn get_page_inks(&self, page_index: usize) -> Result<Vec<String>> {
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        let resources = match page_dict.get("Resources") {
            Some(r) => {
                if let Some(rr) = r.as_reference() {
                    self.load_object(rr)?
                } else {
                    r.clone()
                }
            }
            None => return Ok(Vec::new()),
        };

        let res_dict = match resources.as_dict() {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };

        let cs_obj = match res_dict.get("ColorSpace") {
            Some(obj) => {
                if let Some(r) = obj.as_reference() {
                    self.load_object(r)?
                } else {
                    obj.clone()
                }
            }
            None => return Ok(Vec::new()),
        };

        let cs_dict = match cs_obj.as_dict() {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };

        // Resolve any indirect references so the extractor sees inline
        // arrays. Mirrors the pre-existing per-entry resolve loop.
        let mut resolved: std::collections::HashMap<String, Object> =
            std::collections::HashMap::with_capacity(cs_dict.len());
        for (name, cs_def) in cs_dict.iter() {
            let v = if let Some(r) = cs_def.as_reference() {
                match self.load_object(r) {
                    Ok(o) => o,
                    Err(_) => continue,
                }
            } else {
                cs_def.clone()
            };
            resolved.insert(name.clone(), v);
        }

        let mut ink_names = Vec::new();
        extract_inks_from_color_space_dict(&resolved, Some(self), &mut ink_names);

        ink_names.sort();
        ink_names.dedup();
        Ok(ink_names)
    }

    /// List ink / separation names declared on a page **including** those
    /// declared inside Form XObjects reached through the page's content-stream
    /// `Do` operators.
    ///
    /// Walks the page's content stream looking for `Do` operators that invoke
    /// Form XObjects (§8.10), recurses into each form's `/Resources/ColorSpace`
    /// dictionary, and accumulates `/Separation` and `/DeviceN` ink names from
    /// every visited resource tree.
    ///
    /// **Cycle handling:** indirect XObject references are deduplicated by
    /// `ObjectRef`; recursion depth is bounded at `MAX_RECURSION_DEPTH` (100).
    /// A cycle below the depth bound is silently terminated; a tree deeper
    /// than the bound returns [`Error::RecursionLimitExceeded`].
    ///
    /// **Out of scope:** tiling / shading patterns (§8.7) and annotation
    /// appearance streams (§12.5.5) — both can declare their own colour
    /// spaces but the separation renderer does not paint into them, so
    /// surfacing their inks here would create plates that stay empty.
    pub fn get_page_inks_deep(&self, page_index: usize) -> Result<Vec<String>> {
        let resources = self.page_resources_for_inks(page_index)?;
        let content_data = self.get_page_content_data(page_index)?;
        let operators = crate::content::parser::parse_content_stream(&content_data)?;

        let mut ink_names: Vec<String> = Vec::new();
        let mut visited: std::collections::HashSet<crate::object::ObjectRef> =
            std::collections::HashSet::new();

        self.collect_inks_from_resources(&resources, &mut ink_names)?;
        self.walk_form_xobject_tree_for_inks(
            &operators,
            &resources,
            &mut ink_names,
            &mut visited,
            0,
        )?;

        ink_names.sort();
        ink_names.dedup();
        Ok(ink_names)
    }

    /// Resolve the page's `/Resources` entry, following an indirect
    /// reference if present. Mirrors the same pattern used by
    /// [`Self::get_page_inks`]. Internal helper that does not depend on
    /// the `rendering`-feature-gated [`Self::get_page_resources`].
    fn page_resources_for_inks(&self, page_index: usize) -> Result<Object> {
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;
        let resources = match page_dict.get("Resources") {
            Some(r) => match r.as_reference() {
                Some(rr) => self.load_object(rr)?,
                None => r.clone(),
            },
            None => Object::Dictionary(std::collections::HashMap::new()),
        };
        Ok(resources)
    }

    /// Dereference `obj` if it is an indirect reference; otherwise clone.
    /// Internal helper that mirrors the rendering-gated
    /// [`Self::resolve_object`] without taking the gate.
    fn deref_object_for_inks(&self, obj: &Object) -> Result<Object> {
        match obj.as_reference() {
            Some(r) => self.load_object(r),
            None => Ok(obj.clone()),
        }
    }

    /// Append inks declared in `resources./ColorSpace` (resolving indirect
    /// references) to `out`. Internal helper for both
    /// [`Self::get_page_inks_deep`] and the recursive form walker.
    fn collect_inks_from_resources(&self, resources: &Object, out: &mut Vec<String>) -> Result<()> {
        let res_dict = match resources.as_dict() {
            Some(d) => d,
            None => return Ok(()),
        };
        let cs_obj = match res_dict.get("ColorSpace") {
            Some(obj) => self.deref_object_for_inks(obj)?,
            None => return Ok(()),
        };
        let cs_dict_raw = match cs_obj.as_dict() {
            Some(d) => d,
            None => return Ok(()),
        };

        let mut resolved: std::collections::HashMap<String, Object> =
            std::collections::HashMap::with_capacity(cs_dict_raw.len());
        for (name, cs_def) in cs_dict_raw.iter() {
            let v = match cs_def.as_reference() {
                Some(r) => match self.load_object(r) {
                    Ok(o) => o,
                    Err(_) => continue,
                },
                None => cs_def.clone(),
            };
            resolved.insert(name.clone(), v);
        }
        extract_inks_from_color_space_dict(&resolved, Some(self), out);
        Ok(())
    }

    /// Recursive walker: for every `Operator::Do { name }` in `operators` that
    /// resolves to a Form XObject, scan that form's `/Resources/ColorSpace`
    /// and recurse into the form's own content stream.
    ///
    /// `visited` is keyed on the XObject's `ObjectRef` (indirect references
    /// only). Inline-stream forms cannot self-reference (no name to invoke);
    /// the depth limit is the backstop for any other malformed shape.
    fn walk_form_xobject_tree_for_inks(
        &self,
        operators: &[crate::content::operators::Operator],
        parent_resources: &Object,
        out: &mut Vec<String>,
        visited: &mut std::collections::HashSet<crate::object::ObjectRef>,
        depth: u32,
    ) -> Result<()> {
        if depth >= MAX_RECURSION_DEPTH {
            return Err(Error::RecursionLimitExceeded(MAX_RECURSION_DEPTH));
        }
        let xobjects = match parent_resources.as_dict() {
            Some(rd) => match rd.get("XObject") {
                Some(o) => self.deref_object_for_inks(o)?,
                None => return Ok(()),
            },
            None => return Ok(()),
        };
        let xobj_dict = match xobjects.as_dict() {
            Some(d) => d,
            None => return Ok(()),
        };

        for op in operators {
            let name = match op {
                crate::content::operators::Operator::Do { name } => name,
                _ => continue,
            };
            let xobj_entry = match xobj_dict.get(name) {
                Some(o) => o,
                None => continue,
            };
            let xobj_ref = xobj_entry.as_reference();
            if let Some(r) = xobj_ref {
                // Cycle through indirect refs: silent skip below depth bound.
                if !visited.insert(r) {
                    continue;
                }
            }
            let xobj = match self.deref_object_for_inks(xobj_entry) {
                Ok(o) => o,
                Err(_) => continue,
            };
            let (form_dict, form_stream) = match xobj {
                Object::Stream { ref dict, .. } => {
                    if dict.get("Subtype").and_then(Object::as_name) != Some("Form") {
                        continue;
                    }
                    let data = match xobj_ref {
                        Some(r) => self.decode_stream_with_encryption(&xobj, r)?,
                        None => xobj.decode_stream_data()?,
                    };
                    (dict.clone(), data)
                }
                _ => continue,
            };

            // §8.10.1: form may override resources or inherit the parent's.
            let form_resources = match form_dict.get("Resources") {
                Some(res) => self.deref_object_for_inks(res)?,
                None => parent_resources.clone(),
            };
            self.collect_inks_from_resources(&form_resources, out)?;

            // Recurse into the form's own content stream looking for nested
            // `Do`. Malformed streams are tolerated — we want graceful
            // degradation in a discovery API, not a hard error.
            let form_ops = match crate::content::parser::parse_content_stream(&form_stream) {
                Ok(ops) => ops,
                Err(_) => continue,
            };
            self.walk_form_xobject_tree_for_inks(
                &form_ops,
                &form_resources,
                out,
                visited,
                depth + 1,
            )?;
        }
        Ok(())
    }

    /// # Performance Note
    ///
    /// Character extraction is typically 30-50% faster than span extraction
    /// because it skips the text grouping and merging logic.
    pub fn extract_chars(&self, page_index: usize) -> Result<Vec<crate::layout::TextChar>> {
        Ok((*self.cached_page_chars(page_index)?).clone())
    }

    /// Shared, cached character sequence for a page — identical to what
    /// [`Self::extract_chars`] returns, minus the clone. Only the unfiltered
    /// extraction is cached; the layer/ink-filtered variant is keyed on its
    /// filters and stays uncached.
    fn cached_page_chars(
        &self,
        page_index: usize,
    ) -> Result<std::sync::Arc<Vec<crate::layout::TextChar>>> {
        if let Some(cached) = self.page_chars_cache.lock_or_recover().get(&page_index) {
            return Ok(std::sync::Arc::clone(cached));
        }
        let chars = std::sync::Arc::new(self.extract_chars_impl(
            page_index,
            HashSet::new(),
            HashSet::new(),
        )?);
        self.page_chars_cache
            .lock_or_recover()
            .insert(page_index, std::sync::Arc::clone(&chars));
        Ok(chars)
    }

    /// Extract characters from a page, excluding content from specified layers and inks.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `excluded_layers` - OCG layer names to suppress (empty = no layer filtering)
    /// * `excluded_inks` - Separation/DeviceN ink names to suppress (empty = no ink filtering)
    pub fn extract_chars_filtered(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextChar>> {
        self.extract_chars_impl(page_index, excluded_layers, excluded_inks)
    }

    fn extract_chars_impl(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextChar>> {
        use crate::extractors::TextExtractor;

        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        let content_data = match self.get_page_content_data(page_index) {
            Ok(data) => data,
            Err(e) => {
                log::warn!(
                    "Failed to decode content stream for page {}: {}, returning empty",
                    page_index,
                    e
                );
                return Ok(Vec::new());
            }
        };

        if !Self::may_contain_text(&content_data) {
            return Ok(Vec::new());
        }

        let mut extractor = TextExtractor::new();
        if !excluded_layers.is_empty() {
            extractor.set_excluded_layers(excluded_layers);
        }
        if !excluded_inks.is_empty() {
            extractor.set_excluded_inks(excluded_inks);
        }

        if let Some(resources) = page_dict.get("Resources") {
            extractor.set_resources(resources.clone());
            extractor.set_document(self);
            if let Err(e) = self.load_fonts(resources, &mut extractor) {
                log::warn!(
                    "Failed to load fonts for page {}: {}, continuing with defaults",
                    page_index,
                    e
                );
            }
        }

        let mut chars = extractor.extract_owned(&content_data)?;

        chars.sort_by(|a, b| {
            let y_cmp = crate::utils::safe_float_cmp(b.bbox.y, a.bbox.y);
            if y_cmp != std::cmp::Ordering::Equal {
                return y_cmp;
            }
            crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x)
        });

        Ok(chars)
    }

    /// Extract words from a page.
    ///
    /// Groups characters into words based on spatial proximity.
    /// Uses adaptive thresholds based on the document's font size and spacing.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let words = doc.extract_words(0)?;
    /// for word in words {
    ///     println!("Word: {} at {:?}", word.text, word.bbox);
    /// }
    /// ```
    pub fn extract_words(&self, page_index: usize) -> Result<Vec<crate::layout::Word>> {
        self.extract_words_with_thresholds(page_index, None, None)
    }

    /// Extract words from a page with optional threshold and profile overrides.
    ///
    /// When `word_gap_threshold` is `None`, the adaptive threshold is computed
    /// automatically from page statistics (median character width × 0.3).
    /// Providing a value (in PDF points) overrides the adaptive computation,
    /// which is useful for tuning word segmentation on specific document types.
    ///
    /// When `profile` is provided, it controls how the underlying text spans are
    /// extracted from the PDF content stream (TJ offset thresholds, word margin
    /// ratios). This affects the raw character data before word clustering.
    pub fn extract_words_with_thresholds(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
    ) -> Result<Vec<crate::layout::Word>> {
        // Default: include /Artifact-tagged spans (matches pre-0.3.42
        // behavior). The spec-correct (§14.8.2.2.1) variant lives in
        // [`Self::extract_words_with_thresholds_no_artifacts`].
        self.extract_words_inner(page_index, word_gap_threshold, profile, true)
    }

    /// Same as [`Self::extract_words_with_thresholds`] but drops spans tagged
    /// as `/Artifact` (running headers/footers, page numbers, watermarks;
    /// ISO 32000-1:2008 §14.8.2.2.1). The spec-correct variant.
    pub fn extract_words_with_thresholds_no_artifacts(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
    ) -> Result<Vec<crate::layout::Word>> {
        self.extract_words_inner(page_index, word_gap_threshold, profile, false)
    }

    fn extract_words_inner(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
        include_artifacts: bool,
    ) -> Result<Vec<crate::layout::Word>> {
        use crate::layout::{clustering, AdaptiveLayoutParams, DocumentProperties, Word};

        // Span source. The default (no profile) flows through the canonical
        // `page_reading_order` helper: tagged → struct tree,
        // untagged → geometric top-to-bottom. The legacy profile path keeps
        // its previous XY-Cut + row-aware-sort behavior pending the planned
        // removal of `profile`.
        let spans: Vec<crate::layout::TextSpan> = match profile {
            Some(p) => {
                use crate::pipeline::reading_order::xycut::XYCutStrategy;
                let config = crate::extractors::TextExtractionConfig::new().with_profile(p);
                let mut s = self.extract_spans_raw_with_extraction_config(page_index, config)?;
                s.sort_by(|a, b| {
                    crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
                });
                if !include_artifacts {
                    s.retain(|span| span.artifact_type.is_none());
                }
                let strategy = XYCutStrategy::new();
                strategy
                    .partition_region(&s)
                    .into_iter()
                    .flatten()
                    .collect()
            }
            None => {
                let ordered = if include_artifacts {
                    crate::pipeline::page_reading_order(self, page_index)?
                } else {
                    crate::pipeline::page_reading_order_no_artifacts(self, page_index)?
                };
                ordered.into_iter().map(|os| os.span).collect()
            }
        };
        if spans.is_empty() {
            return Ok(Vec::new());
        }

        // Compute adaptive parameters from all characters for consistent thresholds.
        let media_box = self
            .get_page_media_box(page_index)
            .unwrap_or((0.0, 0.0, 612.0, 792.0));
        let page_bbox =
            crate::geometry::Rect::new(media_box.0, media_box.1, media_box.2, media_box.3);

        // Materialize each span's chars ONCE (to_chars allocates + decodes); the
        // word-clustering loop below reuses chars_per_span instead of calling
        // to_chars a second time per span. Byte-identical, halves to_chars work.
        let mut all_chars: Vec<_> = Vec::new();
        let mut span_char_ranges: Vec<std::ops::Range<usize>> = Vec::with_capacity(spans.len());
        for s in spans.iter() {
            let start = all_chars.len();
            all_chars.extend(s.to_chars());
            span_char_ranges.push(start..all_chars.len());
        }
        if all_chars.is_empty() {
            return Ok(Vec::new());
        }
        let props =
            DocumentProperties::analyze(&all_chars, page_bbox).map_err(Error::LayoutAnalysis)?;
        let mut params = AdaptiveLayoutParams::from_properties(&props);

        // Apply user-provided threshold override
        if let Some(wgt) = word_gap_threshold {
            params.word_gap_threshold = wgt;
        }

        // Walk spans in canonical reading order, clustering chars within each span
        // into words. Since spans come pre-ordered, a flat iteration suffices —
        // no block-by-block partition is needed.
        //
        // Track word indices where the source span had split_boundary_before = true.
        // The post-processing merge must not cross these boundaries (table cells, columns).
        let mut split_boundary_word_indices: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        // Track word indices produced from spans drawn with a rotated text matrix
        // (rotation_degrees != 0 — figure/axis labels, rotated table headers,
        // vertical margin stamps). Such a run's glyphs advance along a rotated
        // axis, but the span bbox flattens them onto the x-axis (width = Σ glyph
        // advances, height = font). Its flattened bbox therefore overlaps
        // unrelated perpendicular columns, and the reading-order-adjacent word
        // merge below would fuse those columns into one giant token (issue #804:
        // a whole rotated column returned as a 1000+ char "word"). Never merge
        // into or out of a rotated run.
        let mut rotated_word_indices: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        let mut words = Vec::new();
        for (span_idx, span) in spans.iter().enumerate() {
            let span_chars = &all_chars[span_char_ranges[span_idx].clone()];
            if span_chars.is_empty() {
                continue;
            }

            // Group characters within THIS SPAN. Since PDF spans are often words or line fragments,
            // this is much safer than global character clustering.
            let clusters =
                clustering::cluster_chars_into_words(span_chars, params.word_gap_threshold);

            // Record split boundary: the first word created from this span is a hard
            // boundary when split_boundary_before = true (e.g. table cell boundary).
            let first_word_idx = words.len();
            let is_split_boundary = span.split_boundary_before;
            let is_rotated_run = span.rotation_degrees != 0.0;

            for cluster_indices in clusters {
                let cluster_chars: Vec<_> = cluster_indices
                    .iter()
                    .map(|&i| span_chars[i].clone())
                    .collect();

                let mut current_word_chars = Vec::new();
                for c in cluster_chars {
                    if c.char.is_whitespace() || c.char == '\n' || c.char == '\r' {
                        if !current_word_chars.is_empty() {
                            let mut word = Word::from_chars(current_word_chars);
                            word.sequence = span.sequence;
                            words.push(word);
                            current_word_chars = Vec::new();
                        }
                    } else {
                        current_word_chars.push(c);
                    }
                }
                if !current_word_chars.is_empty() {
                    let mut word = Word::from_chars(current_word_chars);
                    word.sequence = span.sequence;
                    words.push(word);
                }
            }

            // Only mark the boundary if at least one word was created for this span.
            if is_split_boundary && words.len() > first_word_idx {
                split_boundary_word_indices.insert(first_word_idx);
            }
            // Flag every word from a rotated run so the merge below skips it.
            if is_rotated_run {
                rotated_word_indices.extend(first_word_idx..words.len());
            }
        }

        // Post-processing: merge adjacent words whose spans abut or overlap on
        // the same line. PDFs (especially tagged CJK documents) sometimes encode
        // typographically-adjacent glyphs as separate marked-content runs, e.g.
        // "Q" and "（peu/d）" with a gap of -0.18 points. Without merging these
        // remain separate tokens and never match the ground-truth "Q（peu/d）".
        //
        // Merge condition: same line (y_diff ≤ 0.5 × max line height) AND
        // horizontal gap ≤ 0.15 × font_size (same threshold as should_insert_space).
        // Skip merge when the current word index is a split boundary.
        //
        // `gap` has no lower bound above, so a word that BACKTRACKS far behind
        // the previous word's origin also satisfies `gap ≤ 0.15 × font_size`
        // (a large negative number is always ≤ a small positive one). Displayed
        // math draws a fraction's denominator AFTER the relation sign that
        // follows the numerator (`dx/dt = …` → the `=` is emitted, then `dt`
        // starts ~2em further left at a small baseline offset) — this is the
        // exact geometry `assemble_text_from_spans`'s backtrack branch breaks
        // the line on, just reached here through word bboxes instead of span
        // bboxes. Left unguarded, this loop fuses the pair into `"=dt"`, and
        // because the merge is incremental (`prev` grows to the union bbox),
        // a chain of such backtracks collapses into one word spanning an
        // entire equation — the far worse case reported against `main`.
        // Mirror the emitter's guard: a word that starts at-or-left of the
        // previous word's ORIGIN (not just its end), with a real baseline
        // offset and an overlap far beyond ordinary kerning, is a backtrack,
        // not a same-line neighbour — never merge across it. Gated off for
        // RTL text, whose leftward flow is ordinary reading order.
        let mut merged: Vec<Word> = Vec::with_capacity(words.len());
        // RTL-ness of each entry in `merged`, carried alongside it. `looks_rtl`
        // scans a whole string, and `prev` below GROWS by `push_str` on every
        // merge — re-deriving it per iteration made a chain of k merges cost
        // O(k^2) characters, the same blow-up the backtrack guard above exists
        // to prevent. It is an `any()` over the chars, so
        // `looks_rtl(a + b) == looks_rtl(a) || looks_rtl(b)`: maintain it
        // incrementally instead.
        let mut merged_rtl: Vec<bool> = Vec::with_capacity(words.len());
        let mut prev_rotated = false;
        for (idx, word) in words.into_iter().enumerate() {
            let cur_rotated = rotated_word_indices.contains(&idx);
            let word_rtl = crate::text::bidi::looks_rtl(&word.text);
            if !cur_rotated && !prev_rotated && !split_boundary_word_indices.contains(&idx) {
                if let Some(prev) = merged.last_mut() {
                    let gap = word.bbox.x - (prev.bbox.x + prev.bbox.width);
                    let y_diff = (word.bbox.y - prev.bbox.y).abs();
                    let delta_x = word.bbox.x - prev.bbox.x;
                    let line_h = prev.bbox.height.max(word.bbox.height);
                    let font_size = prev.avg_font_size.max(word.avg_font_size).max(1.0);
                    let not_rtl = !merged_rtl.last().copied().unwrap_or(false) && !word_rtl;
                    let is_math_backtrack =
                        y_diff > 1.0 && delta_x <= 0.5 && gap < -font_size && not_rtl;
                    // A LINE WRAP can land at nearly the same y as the line
                    // above it (some producers emit sub-1pt baseline drift
                    // between consecutive lines, so `y_diff > 1.0` above
                    // doesn't always hold), but it always resets x back
                    // toward the page's left margin — an order of magnitude
                    // further than any real same-line construct (ordinary
                    // kerning is near 0; the math backtrack above is ~1-2em).
                    // A multi-em backtrack this large can only be two
                    // different lines, never a genuine adjacency — reject it
                    // regardless of y_diff, or a wrapped line's tail gets
                    // fused onto its own next line's head (e.g. "of whom" +
                    // "tered with books" → "whomteredwithbooks").
                    let is_line_wrap_reset = delta_x < -5.0 * font_size && not_rtl;
                    if y_diff <= line_h * 0.5
                        && gap <= font_size * 0.15
                        && !is_math_backtrack
                        && !is_line_wrap_reset
                    {
                        // Incremental merge — O(k) per merge, O(total_chars) overall.
                        // Avoids the O(n²) clone+from_chars pattern that caused
                        // catastrophic slowdown on TOC dot-leader pages.
                        let prev_n = prev.chars.len() as f32;
                        let word_n = word.chars.len() as f32;
                        prev.bbox = prev.bbox.union(&word.bbox);
                        prev.avg_font_size = (prev.avg_font_size * prev_n
                            + word.avg_font_size * word_n)
                            / (prev_n + word_n);
                        if word_n > prev_n {
                            prev.dominant_font = word.dominant_font;
                        }
                        prev.is_bold |= word.is_bold;
                        prev.is_italic |= word.is_italic;
                        if prev.mcid != word.mcid {
                            prev.mcid = None;
                        }
                        prev.text.push_str(&word.text);
                        prev.chars.extend(word.chars);
                        if let Some(flag) = merged_rtl.last_mut() {
                            *flag |= word_rtl;
                        }
                        continue;
                    }
                }
            }
            merged.push(word);
            merged_rtl.push(word_rtl);
            prev_rotated = cur_rotated;
        }

        Ok(merged)
    }

    /// Extract text lines from a page.
    ///
    /// Groups words into lines based on vertical proximity.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let lines = doc.extract_text_lines(0)?;
    /// for line in lines {
    ///     println!("Line: {} at {:?}", line.text, line.bbox);
    /// }
    /// ```
    pub fn extract_text_lines(&self, page_index: usize) -> Result<Vec<crate::layout::TextLine>> {
        self.extract_text_lines_with_thresholds(page_index, None, None, None)
    }

    /// Extract text lines from a page with optional threshold and profile overrides.
    ///
    /// When thresholds are `None`, adaptive values are computed automatically
    /// from page statistics. Providing values (in PDF points) overrides the
    /// adaptive computation for fine-grained control over segmentation.
    ///
    /// When `profile` is provided, it controls how the underlying text spans are
    /// extracted from the PDF content stream (TJ offset thresholds, word margin
    /// ratios). This affects the raw character data before word/line clustering.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `word_gap_threshold` - Optional override for the horizontal gap (in PDF points)
    ///   used to split characters into words. Smaller values produce more words.
    /// * `line_gap_threshold` - Optional override for the vertical gap (in PDF points)
    ///   used to group words into lines. Smaller values produce more lines.
    /// * `profile` - Optional extraction profile for span-level tuning.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Use adaptive thresholds (default behavior)
    /// let lines = doc.extract_text_lines_with_thresholds(0, None, None, None)?;
    ///
    /// // Tune both thresholds for dense forms
    /// let lines = doc.extract_text_lines_with_thresholds(0, Some(1.5), Some(4.0), None)?;
    /// ```
    pub fn extract_text_lines_with_thresholds(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        line_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
    ) -> Result<Vec<crate::layout::TextLine>> {
        // Default: include /Artifact-tagged spans (pre-0.3.42 behavior).
        // Spec-correct variant: [`Self::extract_text_lines_with_thresholds_no_artifacts`].
        self.extract_text_lines_inner(
            page_index,
            word_gap_threshold,
            line_gap_threshold,
            profile,
            true,
        )
    }

    /// Same as [`Self::extract_text_lines_with_thresholds`] but drops spans
    /// tagged as `/Artifact` (running headers/footers, page numbers,
    /// watermarks; ISO 32000-1:2008 §14.8.2.2.1). Spec-correct variant.
    pub fn extract_text_lines_with_thresholds_no_artifacts(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        line_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
    ) -> Result<Vec<crate::layout::TextLine>> {
        self.extract_text_lines_inner(
            page_index,
            word_gap_threshold,
            line_gap_threshold,
            profile,
            false,
        )
    }

    fn extract_text_lines_inner(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        line_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
        include_artifacts: bool,
    ) -> Result<Vec<crate::layout::TextLine>> {
        use crate::layout::{clustering, AdaptiveLayoutParams, DocumentProperties, TextLine, Word};

        // Span source. Default (no profile) → canonical `page_reading_order`
        // helper. Legacy profile path keeps XY-Cut + row-aware
        // sort pending the planned removal of `profile`.
        let spans: Vec<crate::layout::TextSpan> = match profile {
            Some(p) => {
                use crate::pipeline::reading_order::xycut::XYCutStrategy;
                let config = crate::extractors::TextExtractionConfig::new().with_profile(p);
                let mut s = self.extract_spans_raw_with_extraction_config(page_index, config)?;
                s.sort_by(|a, b| {
                    crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
                });
                if !include_artifacts {
                    s.retain(|span| span.artifact_type.is_none());
                }
                let strategy = XYCutStrategy::new();
                strategy
                    .partition_region(&s)
                    .into_iter()
                    .flatten()
                    .collect()
            }
            None => {
                let ordered = if include_artifacts {
                    crate::pipeline::page_reading_order(self, page_index)?
                } else {
                    crate::pipeline::page_reading_order_no_artifacts(self, page_index)?
                };
                ordered.into_iter().map(|os| os.span).collect()
            }
        };
        if spans.is_empty() {
            return Ok(Vec::new());
        }

        // Compute adaptive parameters
        let media_box = self
            .get_page_media_box(page_index)
            .unwrap_or((0.0, 0.0, 612.0, 792.0));
        let page_bbox =
            crate::geometry::Rect::new(media_box.0, media_box.1, media_box.2, media_box.3);

        // Materialize each span's chars once (see extract_text_as_words).
        let mut all_chars: Vec<_> = Vec::new();
        let mut span_char_ranges: Vec<std::ops::Range<usize>> = Vec::with_capacity(spans.len());
        for s in spans.iter() {
            let start = all_chars.len();
            all_chars.extend(s.to_chars());
            span_char_ranges.push(start..all_chars.len());
        }
        let props =
            DocumentProperties::analyze(&all_chars, page_bbox).map_err(Error::LayoutAnalysis)?;
        let mut params = AdaptiveLayoutParams::from_properties(&props);

        // Apply user-provided threshold overrides
        if let Some(wgt) = word_gap_threshold {
            params.word_gap_threshold = wgt;
        }
        if let Some(lgt) = line_gap_threshold {
            params.line_gap_threshold = lgt;
        }

        // Walk spans in canonical reading order, clustering chars → words.
        // No block partition; spans are already pre-ordered.
        //
        // `word_rot_run` maps each word to the index of the rotated span it came
        // from (`None` for horizontal spans). A rotated run's glyphs advance
        // along a rotated axis but the span bbox flattens them onto the x-axis,
        // so the flattened y-band line clustering below would fuse the run with
        // its perpendicular neighbours into one giant line (#804). Rotated runs
        // are therefore lifted out and each emitted as its own line.
        let mut words: Vec<Word> = Vec::new();
        let mut word_rot_run: Vec<Option<usize>> = Vec::new();
        for (span_idx, span) in spans.iter().enumerate() {
            let span_chars = &all_chars[span_char_ranges[span_idx].clone()];
            if span_chars.is_empty() {
                continue;
            }
            let rot_run = (span.rotation_degrees != 0.0).then_some(span_idx);

            let clusters =
                clustering::cluster_chars_into_words(span_chars, params.word_gap_threshold);
            for cluster_indices in clusters {
                let cluster_chars: Vec<_> = cluster_indices
                    .iter()
                    .map(|&i| span_chars[i].clone())
                    .collect();
                let mut current_word_chars = Vec::new();
                for c in cluster_chars {
                    if c.char.is_whitespace() || c.char == '\n' || c.char == '\r' {
                        if !current_word_chars.is_empty() {
                            let mut word = Word::from_chars(current_word_chars);
                            word.sequence = span.sequence;
                            words.push(word);
                            word_rot_run.push(rot_run);
                            current_word_chars = Vec::new();
                        }
                    } else {
                        current_word_chars.push(c);
                    }
                }
                if !current_word_chars.is_empty() {
                    let mut word = Word::from_chars(current_word_chars);
                    word.sequence = span.sequence;
                    words.push(word);
                    word_rot_run.push(rot_run);
                }
            }
        }

        if words.is_empty() {
            return Ok(Vec::new());
        }

        // Fast path (byte-identical): no rotated runs on the page → cluster every
        // word by global y-tolerance exactly as before. Same-y words merge into
        // the same line regardless of source span (span ordering already handled
        // the multi-column / structure-tree sequencing upstream).
        if word_rot_run.iter().all(Option::is_none) {
            let line_clusters =
                clustering::cluster_words_into_lines(&words, params.line_gap_threshold);
            let mut all_lines = Vec::new();
            for cluster_indices in line_clusters {
                let cluster_words: Vec<_> =
                    cluster_indices.iter().map(|&i| words[i].clone()).collect();
                all_lines.push(TextLine::new(cluster_words));
            }
            return Ok(all_lines);
        }

        // Rotated-content page: y-band-cluster the horizontal words only, and
        // emit each rotated run as its own line, then restore reading order by
        // the span sequence of each line's first word.
        let horizontal: Vec<Word> = words
            .iter()
            .zip(word_rot_run.iter())
            .filter(|(_, r)| r.is_none())
            .map(|(w, _)| w.clone())
            .collect();
        let mut lines: Vec<Vec<Word>> = Vec::new();
        if !horizontal.is_empty() {
            for cluster_indices in
                clustering::cluster_words_into_lines(&horizontal, params.line_gap_threshold)
            {
                lines.push(
                    cluster_indices
                        .iter()
                        .map(|&i| horizontal[i].clone())
                        .collect(),
                );
            }
        }
        // One line per rotated run (contiguous words sharing the same span index).
        let mut run_start = 0;
        while run_start < words.len() {
            match word_rot_run[run_start] {
                None => run_start += 1,
                Some(run_id) => {
                    let mut run_end = run_start + 1;
                    while run_end < words.len() && word_rot_run[run_end] == Some(run_id) {
                        run_end += 1;
                    }
                    lines.push(words[run_start..run_end].to_vec());
                    run_start = run_end;
                }
            }
        }
        // Reading order: sort lines by the span sequence of their first word
        // (stable so intra-line order is preserved).
        lines.sort_by_key(|line| line.first().map(|w| w.sequence).unwrap_or(usize::MAX));

        Ok(lines.into_iter().map(TextLine::new).collect())
    }

    /// Get the raw content stream data for a page.
    ///
    /// This returns the decoded content stream bytes for the specified page.
    /// The content stream contains PDF operators that define the page's appearance.
    pub fn get_page_content_data(&self, page_index: usize) -> Result<Vec<u8>> {
        Ok((*self.cached_page_content(page_index)?).clone())
    }

    /// Shared, cached content-stream bytes for a page — the same data
    /// [`Self::get_page_content_data`] returns, minus the copy. Extraction
    /// only ever reads the bytes, and a single `extract_words` page touches
    /// this twice (once for spans, once for chars), so handing back the `Arc`
    /// avoids copying the decompressed stream on every call.
    fn cached_page_content(&self, page_index: usize) -> Result<std::sync::Arc<Vec<u8>>> {
        {
            let mut cache = self.page_content_cache.lock_or_recover();
            if let Some(data) = cache.get(&page_index) {
                return Ok(std::sync::Arc::clone(data));
            }
        }

        // Ensure encryption is initialized if needed
        self.ensure_encryption_initialized()?;

        // Get page object
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        // Get content stream(s) — Contents is optional per ISO 32000-1:2008 Table 30
        let contents_ref = match page_dict.get("Contents") {
            Some(Object::Null) | None => {
                log::debug!("Page {} has no /Contents (blank page)", page_index);
                return Ok(std::sync::Arc::new(Vec::new()));
            }
            Some(c) => c,
        };

        // Contents can be either a single stream, an array of streams, or a direct stream object
        let content_data = if let Some(contents_ref_val) = contents_ref.as_reference() {
            // Contents is a reference - it could point to either a Stream or an Array
            let contents = self.load_object(contents_ref_val)?;

            // Check if the loaded object is an Array (indirect array)
            if let Some(contents_array) = contents.as_array() {
                // The reference pointed to an array of streams
                let mut combined = Vec::new();

                for content_item in contents_array.iter() {
                    if matches!(content_item, Object::Null) {
                        continue;
                    }
                    match (|| -> Result<Vec<u8>> {
                        if let Some(ref_val) = content_item.as_reference() {
                            let content_obj = self.load_object(ref_val)?;
                            self.decode_stream_with_encryption(&content_obj, ref_val)
                        } else {
                            content_item.decode_stream_data()
                        }
                    })() {
                        Ok(decoded) => {
                            combined.extend_from_slice(&decoded);
                            combined.push(b'\n');
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to decode content stream element on page {}: {}, skipping",
                                page_index,
                                e
                            );
                        }
                    }
                }

                combined
            } else {
                // The reference pointed to a single stream
                // Decode with encryption support, using the object reference
                self.decode_stream_with_encryption(&contents, contents_ref_val)?
            }
        } else if let Some(contents_array) = contents_ref.as_array() {
            // Array of streams - can be references or direct objects
            let mut combined = Vec::new();

            for content_item in contents_array.iter() {
                if matches!(content_item, Object::Null) {
                    continue;
                }
                match (|| -> Result<Vec<u8>> {
                    if let Some(ref_val) = content_item.as_reference() {
                        let content_obj = self.load_object(ref_val)?;
                        self.decode_stream_with_encryption(&content_obj, ref_val)
                    } else {
                        content_item.decode_stream_data()
                    }
                })() {
                    Ok(decoded) => {
                        combined.extend_from_slice(&decoded);
                        combined.push(b'\n');
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to decode content stream element on page {}: {}, skipping",
                            page_index,
                            e
                        );
                    }
                }
            }

            combined
        } else {
            // Direct stream object (rare but possible)
            // For direct objects, use regular decoding (no encryption key)
            contents_ref.decode_stream_data()?
        };

        log::debug!(
            "Retrieved {} bytes of content data for page {}: {:?}",
            content_data.len(),
            page_index,
            String::from_utf8_lossy(&content_data)
        );

        let content_data = std::sync::Arc::new(content_data);
        self.page_content_cache
            .lock_or_recover()
            .insert(page_index, std::sync::Arc::clone(&content_data));

        Ok(content_data)
    }

    /// Extract path (vector graphics) content from a page.
    ///
    /// This extracts all vector graphics operations from the page's content stream,
    /// including lines, curves, rectangles, and shapes.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Returns
    ///
    /// A vector of `PathContent` objects representing all paths on the page.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("example.pdf")?;
    ///
    /// // Extract paths from first page
    /// let paths = doc.extract_paths(0)?;
    ///
    /// for path in paths {
    ///     println!("Path with {} operations, bbox: {:?}",
    ///         path.operations.len(), path.bbox);
    ///     if path.has_stroke() {
    ///         println!(" Stroked with width: {}", path.stroke_width);
    ///     }
    ///     if path.has_fill() {
    ///         println!(" Filled");
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_paths(&self, page_index: usize) -> Result<Vec<crate::elements::PathContent>> {
        use crate::content::{parse_content_stream_paths_only, Operator};
        use crate::elements::{LineCap, LineJoin};
        use crate::extractors::paths::{FillRule, PathExtractor, PathGraphicsStateStack};
        use crate::layout::Color;

        // Get page object and content stream
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        // Get content stream data — skip page on decode failure (Annex I)
        let content_data = match self.get_page_content_data(page_index) {
            Ok(data) => data,
            Err(e) => {
                log::warn!(
                    "Failed to decode content stream for page {}: {}, returning empty paths",
                    page_index,
                    e
                );
                return Ok(Vec::new());
            }
        };

        let operators = match parse_content_stream_paths_only(&content_data) {
            Ok(ops) => ops,
            Err(e) => {
                log::warn!(
                    "Failed to parse content stream for page {}: {}, returning empty paths",
                    page_index,
                    e
                );
                return Ok(Vec::new());
            }
        };

        let mut extractor = PathExtractor::new();
        let mut state_stack = PathGraphicsStateStack::new();

        // Resolve and set page resources for XObject processing
        if let Some(resources) = page_dict.get("Resources") {
            let resolved_resources = if let Some(ref_obj) = resources.as_reference() {
                self.load_object(ref_obj)?
            } else {
                resources.clone()
            };
            extractor.set_resources(resolved_resources);
        }

        // Process each operator
        for op in operators {
            match op {
                // Graphics state operators
                Operator::SaveState => {
                    state_stack.save();
                }
                Operator::RestoreState => {
                    state_stack.restore();
                    extractor.update_from_path_state(state_stack.current());
                }
                Operator::Cm { a, b, c, d, e, f } => {
                    let state = state_stack.current_mut();
                    let new_matrix = crate::content::Matrix { a, b, c, d, e, f };
                    // PDF spec ISO 32000-1:2008 §8.3.4: cm concatenates as M_cm × CTM
                    state.ctm = new_matrix.multiply(&state.ctm);
                    extractor.set_ctm(state.ctm);
                }

                // Color operators (stroke)
                Operator::SetStrokeRgb { r, g, b } => {
                    state_stack.current_mut().stroke_color_rgb = (r, g, b);
                    extractor.set_stroke_color(Color::new(r, g, b));
                }
                Operator::SetStrokeGray { gray } => {
                    state_stack.current_mut().stroke_color_rgb = (gray, gray, gray);
                    extractor.set_stroke_color(Color::new(gray, gray, gray));
                }
                Operator::SetStrokeCmyk { c, m, y, k } => {
                    let (r, g, b) = crate::color::cmyk_to_rgb(c, m, y, k);
                    state_stack.current_mut().stroke_color_rgb = (r, g, b);
                    extractor.set_stroke_color(Color::new(r, g, b));
                }

                // Color operators (fill)
                Operator::SetFillRgb { r, g, b } => {
                    state_stack.current_mut().fill_color_rgb = (r, g, b);
                    extractor.set_fill_color(Color::new(r, g, b));
                }
                Operator::SetFillGray { gray } => {
                    state_stack.current_mut().fill_color_rgb = (gray, gray, gray);
                    extractor.set_fill_color(Color::new(gray, gray, gray));
                }
                Operator::SetFillCmyk { c, m, y, k } => {
                    let (r, g, b) = crate::color::cmyk_to_rgb(c, m, y, k);
                    state_stack.current_mut().fill_color_rgb = (r, g, b);
                    extractor.set_fill_color(Color::new(r, g, b));
                }

                // Line style operators
                Operator::SetLineWidth { width } => {
                    state_stack.current_mut().line_width = width;
                    extractor.set_line_width(width);
                }
                Operator::SetLineCap { cap_style } => {
                    state_stack.current_mut().line_cap = cap_style;
                    let cap = match cap_style {
                        1 => LineCap::Round,
                        2 => LineCap::Square,
                        _ => LineCap::Butt,
                    };
                    extractor.set_line_cap(cap);
                }
                Operator::SetLineJoin { join_style } => {
                    state_stack.current_mut().line_join = join_style;
                    let join = match join_style {
                        1 => LineJoin::Round,
                        2 => LineJoin::Bevel,
                        _ => LineJoin::Miter,
                    };
                    extractor.set_line_join(join);
                }

                // Path construction operators
                Operator::MoveTo { x, y } => {
                    extractor.move_to(x, y);
                }
                Operator::LineTo { x, y } => {
                    extractor.line_to(x, y);
                }
                Operator::CurveTo {
                    x1,
                    y1,
                    x2,
                    y2,
                    x3,
                    y3,
                } => {
                    extractor.curve_to(x1, y1, x2, y2, x3, y3);
                }
                Operator::CurveToV { x2, y2, x3, y3 } => {
                    extractor.curve_to_v(x2, y2, x3, y3);
                }
                Operator::CurveToY { x1, y1, x3, y3 } => {
                    extractor.curve_to_y(x1, y1, x3, y3);
                }
                Operator::Rectangle {
                    x,
                    y,
                    width,
                    height,
                } => {
                    extractor.rectangle(x, y, width, height);
                }
                Operator::ClosePath => {
                    extractor.close_path();
                }

                // Path painting operators
                Operator::Stroke => {
                    extractor.stroke();
                }
                Operator::Fill => {
                    extractor.fill(FillRule::NonZero);
                }
                Operator::FillEvenOdd => {
                    extractor.fill(FillRule::EvenOdd);
                }
                Operator::CloseFillStroke => {
                    extractor.close_fill_and_stroke(FillRule::NonZero);
                }
                Operator::EndPath => {
                    extractor.end_path();
                }

                // Clipping operators
                Operator::ClipNonZero => {
                    extractor.clip_non_zero();
                }
                Operator::ClipEvenOdd => {
                    extractor.clip_even_odd();
                }

                // XObject processing
                Operator::Do { name } => {
                    if let Err(e) =
                        self.process_form_xobject_paths(&name, &mut extractor, &mut state_stack)
                    {
                        log::warn!(
                            "Failed to process XObject '{}' in path extraction: {}",
                            name,
                            e
                        );
                    }
                }

                // Marked content operators — maintain the active Optional
                // Content Group (PDF "layer") so each finalized path gets
                // tagged with the OCG it was emitted under. Per ISO 32000-1
                // §14.6, every `BDC`/`BMC` must be balanced by an `EMC`,
                // so we always push (with `None` for non-`/OC` tags) and
                // always pop — keeps the stack depth in sync with the
                // marked-content nesting.
                Operator::BeginMarkedContent { .. } => {
                    extractor.push_oc_layer(None);
                }
                Operator::BeginMarkedContentDict { tag, properties } => {
                    let layer = if tag == "OC" {
                        self.resolve_oc_layer_name(extractor.current_resources(), &properties)
                    } else {
                        None
                    };
                    extractor.push_oc_layer(layer);
                }
                Operator::EndMarkedContent => {
                    extractor.pop_oc_layer();
                }

                // Skip other operators (text, images, etc.)
                _ => {}
            }
        }

        Ok(extractor.finish())
    }

    /// Resolve a `BDC /OC <properties>` property operand to the human-readable
    /// layer name of the Optional Content it refers to (PDF spec
    /// ISO 32000-1:2008 §8.11, §14.6).
    ///
    /// `properties` is the operand parsed by `Operator::BeginMarkedContentDict`
    /// — per spec it is either:
    ///
    /// 1. An inline dictionary: an OCG (or OCMD) — read its name directly.
    /// 2. A name (e.g. `/MC0`) that references `<resources> /Properties
    ///    <name>` → an OCG or OCMD dictionary → read its name.
    ///
    /// `resources` is the resource dictionary currently in scope: the page
    /// `/Resources` at page level, or the active Form XObject's own
    /// `/Resources` when extracting inside an XObject (§14.6.2, §8.10.1).
    ///
    /// Returns `None` for malformed PDFs, missing `/Resources /Properties`
    /// entries, or optional-content objects without a resolvable name.
    /// Callers treat `None` as "path belongs to no named layer" — extraction
    /// continues normally.
    fn resolve_oc_layer_name(
        &self,
        resources: Option<&crate::object::Object>,
        properties: &crate::object::Object,
    ) -> Option<String> {
        const OC_NAME_MAX_DEPTH: u8 = 8;

        // Case 1: inline dictionary — the property list itself is the OCG (or
        // OCMD) dictionary.
        if let Some(dict) = properties.as_dict() {
            return self.read_oc_name(dict, OC_NAME_MAX_DEPTH);
        }

        // Case 2: name reference (e.g. `/MC0`) — resolve through the current
        // resource dict's `/Properties` subdictionary.
        let prop_name = properties.as_name()?;
        let resources_obj = self.deref_object(resources?)?;
        let properties_dict = resources_obj.as_dict()?.get("Properties")?;
        let properties_obj = self.deref_object(properties_dict)?;
        let target = properties_obj.as_dict()?.get(prop_name)?;
        let target_obj = self.deref_object(target)?;
        self.read_oc_name(target_obj.as_dict()?, OC_NAME_MAX_DEPTH)
    }

    /// Read the human-readable layer name from an Optional Content dictionary.
    ///
    /// - An **OCG** (§8.11.2.1) carries its label in `/Name` — a PDF *text
    ///   string*, decoded via [`Self::decode_pdf_text_string`] so
    ///   PDFDocEncoding (Annex D) and UTF-16 (BE/LE, with BOM) layer names
    ///   round-trip identically to the rest of the library.
    /// - An **OCMD** (§8.11.3.2, Table 99) has no `/Name` of its own; its
    ///   member OCGs live in `/OCGs`, which is *either* a single OCG *or* an
    ///   array of them (array entries may be `null`). We follow the first
    ///   entry that resolves to a dictionary and read its name.
    ///
    /// `depth` bounds the `/OCGs` chain so a malformed PDF whose membership
    /// dictionary points back to another OCMD cannot recurse forever.
    /// Returns `None` for missing / non-dictionary / nameless inputs — the
    /// path is simply left unlabelled.
    fn read_oc_name(
        &self,
        dict: &std::collections::HashMap<String, crate::object::Object>,
        depth: u8,
    ) -> Option<String> {
        use crate::object::Object;

        if depth == 0 {
            return None;
        }

        // OCMD: no /Name of its own — follow /OCGs to the first member OCG.
        if matches!(dict.get("Type").and_then(|t| t.as_name()), Some("OCMD")) {
            let ocgs = self.deref_object(dict.get("OCGs")?)?;
            let first_ocg = match ocgs.as_array() {
                // /OCGs as an array: first entry that derefs to a dictionary.
                Some(entries) => entries
                    .iter()
                    .find_map(|e| self.deref_object(e).filter(|o| o.as_dict().is_some())),
                // /OCGs as a single OCG (already a dictionary).
                None => Some(ocgs.clone()),
            };
            return self.read_oc_name(first_ocg?.as_dict()?, depth - 1);
        }

        // OCG (or inline property dict): /Name is a PDF text string.
        match dict.get("Name")? {
            Object::String(bytes) => Some(Self::decode_pdf_text_string(bytes)),
            // Tolerate a /Name written as a PDF name object (non-conformant,
            // but seen in real exports).
            Object::Name(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// Dereference one level of indirection, loading the target object;
    /// pass direct objects through unchanged. `None` if a reference fails to
    /// load — callers treat that as "unresolvable, leave unlabelled".
    fn deref_object(&self, obj: &crate::object::Object) -> Option<crate::object::Object> {
        match obj.as_reference() {
            Some(r) => self.load_object(r).ok(),
            None => Some(obj.clone()),
        }
    }

    /// Extract rectangles from a page (v0.3.14).
    ///
    /// Identifies paths that form axis-aligned rectangles.
    pub fn extract_rects(&self, page_index: usize) -> Result<Vec<crate::elements::PathContent>> {
        let paths = self.extract_paths(page_index)?;
        Ok(paths.into_iter().filter(|p| p.is_rectangle()).collect())
    }

    /// Extract straight lines from a page (v0.3.14).
    ///
    /// Identifies paths that form a single straight line segment.
    pub fn extract_lines(&self, page_index: usize) -> Result<Vec<crate::elements::PathContent>> {
        let paths = self.extract_paths(page_index)?;
        Ok(paths.into_iter().filter(|p| p.is_straight_line()).collect())
    }

    /// Extract tables from a page (v0.3.14).
    ///
    /// Uses a hybrid spatial algorithm that combines text alignment and vector lines
    /// for robust table detection without explicit structure markup.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let tables = doc.extract_tables(0)?;
    /// for table in tables {
    ///     println!("Table with {} rows and {} columns", table.rows.len(), table.col_count);
    /// }
    /// ```
    pub fn extract_tables(
        &self,
        page_index: usize,
    ) -> Result<Vec<crate::structure::table_extractor::Table>> {
        self.extract_tables_with_config(
            page_index,
            crate::structure::spatial_table_detector::TableDetectionConfig::default(),
        )
    }

    /// Extract tables from a page using a custom configuration (v0.3.14).
    pub fn extract_tables_with_config(
        &self,
        page_index: usize,
        config: crate::structure::spatial_table_detector::TableDetectionConfig,
    ) -> Result<Vec<crate::structure::table_extractor::Table>> {
        use crate::structure::spatial_table_detector::detect_tables_with_lines;

        // Use words instead of spans for better granularity.
        // This ensures that strings with spaces are split into separate columns
        // for the spatial detector.
        let words = self.extract_words(page_index)?;
        // Use all table primitives (lines, rectangles, borders) not just straight lines
        let lines: Vec<_> = self
            .extract_paths(page_index)?
            .into_iter()
            .filter(|p| p.is_table_primitive())
            .collect();

        // Convert Words to TextSpans for the spatial detector
        let spans: Vec<_> = words
            .into_iter()
            .map(|w| crate::layout::TextSpan {
                provenance: None,
                artifact_type: None,
                text: w.text,
                bbox: w.bbox,
                font_name: w.dominant_font,
                font_size: w.avg_font_size,
                font_weight: if w.is_bold {
                    crate::layout::FontWeight::Bold
                } else {
                    crate::layout::FontWeight::Normal
                },
                is_italic: w.is_italic,
                is_monospace: false,
                color: crate::layout::Color::black(),
                mcid: w.mcid,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 1.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                text_rise: 0.0,
                rtl_draw_logical: false,
            })
            .collect();

        // Same prose-rejection filter `extract_page_tables` applies to the
        // extract_text/to_markdown/to_html path — this public API called
        // `detect_tables_with_lines` directly with no post-filter at all, so
        // it was already able to fabricate/garble tables on any prose-shaped
        // spatial candidate, independent of anything else in this function.
        Ok(detect_tables_with_lines(&spans, &lines, &config)
            .into_iter()
            .filter(|t| t.is_real_grid() && !looks_like_prose_table(t))
            .collect())
    }

    /// Process paths from a Form XObject.
    ///
    /// This method recursively extracts paths from Form XObjects encountered via the `Do` operator.
    /// It handles:
    /// - XObject resolution from resources
    /// - Type checking (Form vs Image)
    /// - Stream decoding and operator parsing
    /// - Coordinate transformations via /Matrix
    /// - Graphics state isolation
    ///
    /// # Arguments
    ///
    /// * `name` - The XObject name from the `Do` operator
    /// * `extractor` - The path extractor to accumulate paths
    /// * `state_stack` - The graphics state stack for transformations
    fn process_form_xobject_paths(
        &self,
        name: &str,
        extractor: &mut crate::extractors::paths::PathExtractor,
        state_stack: &mut crate::extractors::paths::PathGraphicsStateStack,
    ) -> Result<()> {
        use crate::content::{parse_content_stream_paths_only, Matrix, Operator};
        use crate::elements::{LineCap, LineJoin};
        use crate::extractors::paths::FillRule;
        use crate::layout::Color;

        let xobject_ref =
            match extractor.resolve_xobject_ref(name, |ref_obj| self.load_object(ref_obj)) {
                Some(r) => r,
                None => return Ok(()),
            };

        // Cycle detection
        if !extractor.can_process_xobject(xobject_ref) {
            return Ok(());
        }
        extractor.push_xobject(xobject_ref);

        // Load XObject
        let xobject = match self.load_object(xobject_ref) {
            Ok(obj) => obj,
            Err(e) => {
                extractor.pop_xobject_failed();
                return Err(e);
            }
        };
        let xobject_dict = match xobject.as_dict() {
            Some(dict) => dict,
            None => {
                extractor.pop_xobject_failed();
                return Err(Error::ParseError {
                    offset: 0,
                    reason: "XObject is not a dictionary".to_string(),
                });
            }
        };

        // Check type - only process Form XObjects, skip Images
        match xobject_dict.get("Subtype") {
            Some(subtype_obj) => {
                if let Some(subtype_name) = subtype_obj.as_name() {
                    if subtype_name != "Form" {
                        extractor.pop_xobject();
                        return Ok(()); // Not a Form XObject, skip
                    }
                } else {
                    extractor.pop_xobject();
                    return Ok(());
                }
            }
            None => {
                extractor.pop_xobject();
                return Ok(());
            }
        }

        // Decode stream — reuse document-level cache shared with text extraction.
        let cached_stream = {
            self.xobject_stream_cache
                .lock_or_recover()
                .get(&xobject_ref)
                .cloned()
        };
        let stream_data = if let Some(cached) = cached_stream {
            cached.as_ref().clone()
        } else {
            match self.decode_stream_with_encryption(&xobject, xobject_ref) {
                Ok(data) => {
                    const MAX_STREAM_CACHE_BYTES: usize = 50 * 1024 * 1024;
                    let current = self.xobject_stream_cache_bytes.load(Ordering::Relaxed);
                    if current + data.len() <= MAX_STREAM_CACHE_BYTES {
                        self.xobject_stream_cache_bytes
                            .store(current + data.len(), Ordering::Relaxed);
                        self.xobject_stream_cache
                            .lock_or_recover()
                            .insert(xobject_ref, std::sync::Arc::new(data.clone()));
                    }
                    data
                }
                Err(e) => {
                    extractor.pop_xobject_failed();
                    return Err(e);
                }
            }
        };

        let operators = match parse_content_stream_paths_only(&stream_data) {
            Ok(ops) => ops,
            Err(e) => {
                extractor.pop_xobject_failed();
                return Err(e);
            }
        };

        // Get transformation matrix (default to identity)
        let matrix = if let Some(matrix_obj) = xobject_dict.get("Matrix") {
            if let Some(array) = matrix_obj.as_array() {
                if array.len() >= 6 {
                    let mut matrix = Matrix::identity();
                    let mut values = [0.0f32; 6];
                    let mut valid = true;

                    for (i, val) in array.iter().take(6).enumerate() {
                        let num = if let Some(f) = val.as_real() {
                            f as f32
                        } else if let Some(i_val) = val.as_integer() {
                            i_val as f32
                        } else {
                            valid = false;
                            break;
                        };
                        values[i] = num;
                    }

                    if valid {
                        matrix.a = values[0];
                        matrix.b = values[1];
                        matrix.c = values[2];
                        matrix.d = values[3];
                        matrix.e = values[4];
                        matrix.f = values[5];
                        matrix
                    } else {
                        Matrix::identity()
                    }
                } else {
                    Matrix::identity()
                }
            } else {
                Matrix::identity()
            }
        } else {
            Matrix::identity()
        };

        // Save graphics state
        state_stack.save();

        // Finalize any pending path before processing XObject to isolate state
        if extractor.has_current_path() {
            extractor.end_path();
        }

        // Apply XObject transformation to CTM
        // PDF spec ISO 32000-1:2008 §8.10.1: Form XObject Matrix concatenates as M × CTM
        let state = state_stack.current_mut();
        state.ctm = matrix.multiply(&state.ctm);
        extractor.set_ctm(state.ctm);

        // Switch resource scope to this Form XObject's own /Resources, if any.
        // Form XObjects with their own Resources define a fresh XObject name
        // scope (ISO 32000-1 §8.10.1). Looking up nested `Do` names against the
        // parent scope can pick up unrelated sibling forms with colliding
        // names, which turns sibling Form XObjects into a cross-recursive tree
        // (O(N!) traversals and unbounded path accumulation).
        let saved_scope = if let Some(xobj_resources) = xobject_dict.get("Resources") {
            let resolved = if let Some(res_ref) = xobj_resources.as_reference() {
                self.load_object(res_ref)
                    .unwrap_or_else(|_| xobj_resources.clone())
            } else {
                xobj_resources.clone()
            };
            Some(extractor.swap_resources(Some(resolved)))
        } else {
            None
        };

        // Remember the marked-content nesting depth on entry so we can drop
        // anything this XObject leaves unbalanced (see truncate below).
        let oc_base_depth = extractor.oc_layer_depth();

        // Process operators from the XObject
        for op in operators {
            match op {
                // Graphics state operators
                Operator::SaveState => {
                    state_stack.save();
                }
                Operator::RestoreState => {
                    state_stack.restore();
                    extractor.update_from_path_state(state_stack.current());
                }
                Operator::Cm { a, b, c, d, e, f } => {
                    let state = state_stack.current_mut();
                    let new_matrix = Matrix { a, b, c, d, e, f };
                    // PDF spec ISO 32000-1:2008 §8.3.4: cm concatenates as M_cm × CTM
                    state.ctm = new_matrix.multiply(&state.ctm);
                    extractor.set_ctm(state.ctm);
                }

                // Color and line style operators — must update both state_stack
                // and extractor so q/Q save/restore works correctly.
                Operator::SetStrokeRgb { r, g, b } => {
                    state_stack.current_mut().stroke_color_rgb = (r, g, b);
                    extractor.set_stroke_color(Color::new(r, g, b));
                }
                Operator::SetStrokeGray { gray } => {
                    state_stack.current_mut().stroke_color_rgb = (gray, gray, gray);
                    extractor.set_stroke_color(Color::new(gray, gray, gray));
                }
                Operator::SetStrokeCmyk { c, m, y, k } => {
                    let (r, g, b) = crate::color::cmyk_to_rgb(c, m, y, k);
                    state_stack.current_mut().stroke_color_rgb = (r, g, b);
                    extractor.set_stroke_color(Color::new(r, g, b));
                }
                Operator::SetFillRgb { r, g, b } => {
                    state_stack.current_mut().fill_color_rgb = (r, g, b);
                    extractor.set_fill_color(Color::new(r, g, b));
                }
                Operator::SetFillGray { gray } => {
                    state_stack.current_mut().fill_color_rgb = (gray, gray, gray);
                    extractor.set_fill_color(Color::new(gray, gray, gray));
                }
                Operator::SetFillCmyk { c, m, y, k } => {
                    let (r, g, b) = crate::color::cmyk_to_rgb(c, m, y, k);
                    state_stack.current_mut().fill_color_rgb = (r, g, b);
                    extractor.set_fill_color(Color::new(r, g, b));
                }
                Operator::SetLineWidth { width } => {
                    state_stack.current_mut().line_width = width;
                    extractor.set_line_width(width);
                }
                Operator::SetLineCap { cap_style } => {
                    state_stack.current_mut().line_cap = cap_style;
                    let cap = match cap_style {
                        1 => LineCap::Round,
                        2 => LineCap::Square,
                        _ => LineCap::Butt,
                    };
                    extractor.set_line_cap(cap);
                }
                Operator::SetLineJoin { join_style } => {
                    state_stack.current_mut().line_join = join_style;
                    let join = match join_style {
                        1 => LineJoin::Round,
                        2 => LineJoin::Bevel,
                        _ => LineJoin::Miter,
                    };
                    extractor.set_line_join(join);
                }

                // Path construction operators
                Operator::MoveTo { x, y } => extractor.move_to(x, y),
                Operator::LineTo { x, y } => extractor.line_to(x, y),
                Operator::CurveTo {
                    x1,
                    y1,
                    x2,
                    y2,
                    x3,
                    y3,
                } => {
                    extractor.curve_to(x1, y1, x2, y2, x3, y3);
                }
                Operator::CurveToV { x2, y2, x3, y3 } => {
                    extractor.curve_to_v(x2, y2, x3, y3);
                }
                Operator::CurveToY { x1, y1, x3, y3 } => {
                    extractor.curve_to_y(x1, y1, x3, y3);
                }
                Operator::Rectangle {
                    x,
                    y,
                    width,
                    height,
                } => {
                    extractor.rectangle(x, y, width, height);
                }
                Operator::ClosePath => extractor.close_path(),

                // Path painting operators
                Operator::Stroke => extractor.stroke(),
                Operator::Fill => extractor.fill(FillRule::NonZero),
                Operator::FillEvenOdd => extractor.fill(FillRule::EvenOdd),
                Operator::CloseFillStroke => extractor.close_fill_and_stroke(FillRule::NonZero),
                Operator::EndPath => extractor.end_path(),

                // Clipping operators
                Operator::ClipNonZero => extractor.clip_non_zero(),
                Operator::ClipEvenOdd => extractor.clip_even_odd(),

                // Nested XObjects (recurse)
                Operator::Do { name: nested_name } => {
                    if let Err(e) =
                        self.process_form_xobject_paths(&nested_name, extractor, state_stack)
                    {
                        log::warn!("Failed to process nested XObject '{}': {}", nested_name, e);
                    }
                }

                // Marked content — same Optional Content Group ("layer")
                // tracking as the page-level loop, but `/OC` property
                // references resolve against *this* XObject's resource scope
                // (swapped in above), per §14.6.2 + §8.10.1. CAD exports that
                // reuse Form XObjects for repeated symbols (gridline labels,
                // callouts) carry their `/OC` markers and local `/Properties`
                // here rather than on the page.
                Operator::BeginMarkedContent { .. } => {
                    extractor.push_oc_layer(None);
                }
                Operator::BeginMarkedContentDict { tag, properties } => {
                    let layer = if tag == "OC" {
                        self.resolve_oc_layer_name(extractor.current_resources(), &properties)
                    } else {
                        None
                    };
                    extractor.push_oc_layer(layer);
                }
                Operator::EndMarkedContent => {
                    extractor.pop_oc_layer();
                }

                // Skip other operators
                _ => {}
            }
        }

        // Finalize any pending path to prevent state leakage
        if extractor.has_current_path() {
            extractor.end_path();
        }

        // Drop any marked-content entries this XObject left open so an
        // unbalanced `BDC` cannot leak its layer onto the caller's paths.
        extractor.truncate_oc_layers(oc_base_depth);

        // Restore the caller's resource scope before popping the cycle guard.
        if let Some(saved) = saved_scope {
            extractor.restore_resources(saved);
        }

        // Restore graphics state
        state_stack.restore();
        extractor.update_from_path_state(state_stack.current());

        // Pop from XObject processing stack
        extractor.pop_xobject();

        Ok(())
    }

    /// Extract paths from a specific rectangular region of a page.
    ///
    /// Only paths whose bounding box intersects the specified region are returned.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `region` - The rectangular region to extract from
    ///
    /// # Returns
    ///
    /// A vector of `PathContent` objects within the specified region.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # use pdf_oxide::geometry::Rect;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("example.pdf")?;
    ///
    /// // Extract paths from a specific region (e.g., header area)
    /// let header_region = Rect::new(0.0, 700.0, 612.0, 92.0);
    /// let paths = doc.extract_paths_in_rect(0, header_region)?;
    ///
    /// println!("Found {} paths in header region", paths.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_paths_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::elements::PathContent>> {
        let paths = self.extract_paths(page_index)?;

        // Filter paths by region intersection against RENDERED extents: a
        // region query answers "what does the reader see here", so a rule
        // whose drawn bar crosses the region must match even when its
        // geometric bbox is a distant speck. Identical to the
        // geometric test for unstroked paths.
        Ok(paths
            .into_iter()
            .filter(|path| path.rendered_bbox().intersects(&region))
            .collect())
    }

    /// Extract text from a specific rectangular region of a page (v0.3.14).
    ///
    /// Only spans whose bounding boxes match `region` under `mode` are kept;
    /// the retained spans are assembled through the full text pipeline
    /// (reading order, tables, line breaks) so the output matches the
    /// quality of [`Self::extract_text`]. Calling this with a region that covers
    /// the whole page is equivalent to [`Self::extract_text`].
    pub fn extract_text_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<String> {
        let options = crate::converters::ConversionOptions {
            extract_tables: true,
            include_region: Some((region, mode)),
            ..Default::default()
        };
        self.extract_text_with_options(page_index, &options)
    }

    /// Extract words from a specific rectangular region of a page (v0.3.14).
    pub fn extract_words_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::Word>> {
        use crate::layout::SpatialCollectionFiltering;
        let words = self.extract_words(page_index)?;
        Ok(words.filter_by_rect(&region, mode))
    }

    /// Extract text lines from a specific rectangular region of a page (v0.3.14).
    pub fn extract_text_lines_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::TextLine>> {
        use crate::layout::SpatialCollectionFiltering;
        let lines = self.extract_text_lines(page_index)?;
        Ok(lines.filter_by_rect(&region, mode))
    }

    /// Extract text spans from a specific rectangular region of a page (v0.3.14).
    pub fn extract_spans_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        use crate::layout::SpatialCollectionFiltering;
        let spans = self.extract_spans(page_index)?;
        Ok(spans.filter_by_rect(&region, mode))
    }

    /// Extract text from a page excluding specific rectangular regions.
    ///
    /// The excluded spans are removed before the full text-assembly pipeline
    /// runs, so the output has the same structure — line breaks, tables,
    /// reading order — as [`Self::extract_text`]. Calling this with an empty
    /// `exclude` slice is equivalent to [`Self::extract_text`].
    ///
    /// `mode` controls the overlap rule:
    /// - [`crate::layout::RectFilterMode::Intersects`] (default): drop any span with *any* overlap
    /// - [`crate::layout::RectFilterMode::FullyContained`]: drop only spans lying entirely inside
    /// - `RectFilterMode::MinOverlap(t)`: drop spans where at least fraction `t`
    ///   of the *span's* area overlaps an excluded region
    ///
    /// For Tagged PDFs the extractor already honours `/Artifact` marked-content
    /// (PDF spec §14.8.2.2). This method provides the same capability for
    /// untagged PDFs where spatial coordinates are the only available signal.
    /// Exclusion is unconditional: spans inside a region are dropped regardless
    /// of their structure-tree role.
    pub fn extract_text_excluding_rects(
        &self,
        page_index: usize,
        exclude: &[crate::geometry::Rect],
        mode: crate::layout::RectFilterMode,
    ) -> Result<String> {
        let options = crate::converters::ConversionOptions {
            extract_tables: true,
            exclude_regions: exclude.to_vec(),
            exclude_regions_mode: mode,
            ..Default::default()
        };
        self.extract_text_with_options(page_index, &options)
    }

    /// Extract words from a page excluding specific rectangular regions.
    ///
    /// See [`Self::extract_text_excluding_rects`] for a description of `exclude` and `mode`.
    /// Returns the low-level [`crate::layout::Word`] stream; use [`Self::extract_text_excluding_rects`]
    /// for fully-assembled text with line breaks and tables.
    pub fn extract_words_excluding_rects(
        &self,
        page_index: usize,
        exclude: &[crate::geometry::Rect],
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::Word>> {
        use crate::layout::SpatialCollectionFiltering;
        let words = self.extract_words(page_index)?;
        Ok(words.exclude_rects(exclude, mode))
    }

    /// Extract text spans from a page excluding specific rectangular regions.
    ///
    /// See [`Self::extract_text_excluding_rects`] for a description of `exclude` and `mode`.
    /// Returns raw [`crate::layout::TextSpan`] objects with bounding boxes and font metadata;
    /// use [`Self::extract_text_excluding_rects`] for fully-assembled text output.
    pub fn extract_spans_excluding_rects(
        &self,
        page_index: usize,
        exclude: &[crate::geometry::Rect],
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        use crate::layout::SpatialCollectionFiltering;
        let spans = self.extract_spans(page_index)?;
        Ok(spans.exclude_rects(exclude, mode))
    }

    /// Extract rectangles from a specific rectangular region of a page (v0.3.14).
    pub fn extract_rects_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::elements::PathContent>> {
        let rects = self.extract_rects(page_index)?;
        // Rendered extents, matching `extract_paths_in_rect`.
        Ok(rects
            .into_iter()
            .filter(|p| p.rendered_bbox().intersects(&region))
            .collect())
    }

    /// Extract straight lines from a specific rectangular region of a page (v0.3.14).
    pub fn extract_lines_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::elements::PathContent>> {
        let lines = self.extract_lines(page_index)?;
        // Rendered extents, matching `extract_paths_in_rect`: a
        // stroke-width-encoded rule must match region queries over its drawn
        // bar, not only over its geometric speck.
        Ok(lines
            .into_iter()
            .filter(|p| p.rendered_bbox().intersects(&region))
            .collect())
    }

    /// Extract individual characters from a specific rectangular region of a page (v0.3.14).
    pub fn extract_chars_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::TextChar>> {
        use crate::layout::SpatialCollectionFiltering;
        let chars = self.extract_chars(page_index)?;
        Ok(chars.filter_by_rect(&region, mode))
    }

    /// Extract images from a specific rectangular region of a page (v0.3.14).
    pub fn extract_images_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::extractors::PdfImage>> {
        let images = self.extract_images(page_index)?;
        Ok(images
            .into_iter()
            .filter(|img| {
                if let Some(bbox) = img.bbox() {
                    bbox.intersects(&region)
                } else {
                    false
                }
            })
            .collect())
    }

    /// Extract tables from a specific rectangular region of a page (v0.3.14).
    pub fn extract_tables_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::structure::table_extractor::Table>> {
        self.extract_tables_in_rect_with_config(
            page_index,
            region,
            crate::structure::spatial_table_detector::TableDetectionConfig::relaxed(),
        )
    }

    /// Extract tables from a specific region using custom configuration (v0.3.14).
    pub fn extract_tables_in_rect_with_config(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        config: crate::structure::spatial_table_detector::TableDetectionConfig,
    ) -> Result<Vec<crate::structure::table_extractor::Table>> {
        let tables = self.extract_tables_with_config(page_index, config)?;
        Ok(tables
            .into_iter()
            .filter(|table| {
                if let Some(bbox) = table.bbox {
                    bbox.intersects(&region)
                } else {
                    false
                }
            })
            .collect())
    }

    /// Compute a cheap content-based font identity hash from a loaded font object.
    /// Uses only inline fields (no reference resolution / load_object calls) to keep
    /// the cost at ~200ns. Relies on BaseFont + Subtype + Encoding (when inline) to
    /// uniquely identify fonts within a document. For reference-only fields (ToUnicode,
    /// FontDescriptor, DescendantFonts), hashes their presence to avoid false positives
    /// between fonts with vs without these features.
    /// `font_identity_hash_cheap` of `font_ref`'s object, memoized (an object's
    /// content is fixed within a document).
    fn cached_font_identity_hash(&self, font_ref: ObjectRef) -> Option<u64> {
        if let Some(&h) = self.font_id_hash_cache.lock_or_recover().get(&font_ref) {
            return Some(h);
        }
        let font = self.load_object(font_ref).ok()?;
        let h = self.font_identity_hash_with_descendants(&font);
        self.font_id_hash_cache
            .lock_or_recover()
            .insert(font_ref, h);
        Some(h)
    }

    /// Document-aware extension of `font_identity_hash_cheap` that folds the
    /// *content* of a font's document-specific streams — its `/ToUnicode` CMap
    /// and embedded font program(s) — plus the descendant CIDFont's width
    /// metrics (`/DW`, `/DW2`, `/W`, `/W2`) and stream-form `/CIDToGIDMap` into
    /// the identity hash.
    ///
    /// Why content, not just references: `font_identity_hash_cheap` folds only
    /// the *reference* (object id/gen) of `/ToUnicode`, and the global cache is
    /// skipped only for *canonical* subset fonts (`AAAAAA+`, six uppercase
    /// letters + `+`; see `is_subset_basefont`). A non-canonical subset tag
    /// such as `/CIDFont+F1` is therefore still shared cross-document, and
    /// PDFs emitted from a common template reuse the same `/ToUnicode` object
    /// number — so two genuinely different fonts that merely share a
    /// `/BaseFont` name produce an identical cheap hash. Keyed only by that
    /// hash, the cross-document global cache (Layer 6) served a later document
    /// the *earlier* font's parsed `FontInfo`, and its glyph→Unicode mapping
    /// came out as a constant-offset cipher or control/PUA junk (e.g.
    /// `SUMMARY` → `6800$5<`). Folding the `/ToUnicode` stream bytes — and the
    /// embedded `/FontFile{,2,3}` bytes — gives such fonts distinct keys so
    /// they can never collide regardless of subset-tag form or object reuse,
    /// while genuinely identical fonts still dedup. This completes the
    /// cross-document hardening from #595/#597/#598 (which folded the
    /// `/ToUnicode` *reference* and the `/Widths`, and excluded canonical
    /// `AAAAAA+` subsets), applied to the field that actually decodes text.
    ///
    /// Cost: a few extra `load_object` calls (the `/ToUnicode` stream, each
    /// descendant CIDFont, the `/FontDescriptor`s and their font programs) on
    /// the first encounter of a font per document; subsequent calls hit
    /// `font_id_hash_cache`, and the loads themselves are served from the
    /// object cache that `FontInfo::from_dict` populates anyway. Stream bytes
    /// are folded *raw* (still encoded) — see `fold_stream_bytes`.
    fn font_identity_hash_with_descendants(&self, font_obj: &Object) -> u64 {
        use std::hash::{Hash, Hasher};
        // Seed with the cheap inline hash so existing identity coverage is
        // preserved bit-for-bit when there are no streams/descendants to fold.
        let base = Self::font_identity_hash_cheap(font_obj);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        base.hash(&mut hasher);

        if let Some(d) = font_obj.as_dict() {
            // /ToUnicode stream BYTES — the decisive discriminator. The cheap
            // hash folds only this stream's reference; folding its content is
            // what stops same-named, differently-mapped fonts from colliding
            // across documents when the cheap key matches (#595).
            if let Some(to_unicode) = d.get("ToUnicode") {
                17u8.hash(&mut hasher);
                self.fold_stream_bytes(to_unicode, &mut hasher);
            }

            // /Encoding CONTENT for a referenced or inline encoding dictionary.
            // The cheap hash can only fold a constant marker for a
            // referenced/dict /Encoding (it does not resolve), so two simple
            // fonts that share a /BaseFont name but re-encode through DIFFERENT
            // /Differences arrays collide on the cheap key. That is common in
            // subsetted PDFs: several /BaseFont "Times-Roman" instances each
            // carry a per-instance, frequency-ordered /Encoding (code 1 -> first
            // glyph used, ...). With no /ToUnicode and no embedded program (a
            // non-embedded base font) NOTHING else distinguishes them, so the
            // per-document cache served the first font's parsed FontInfo for the
            // second, decoding its body text through the wrong /Differences - a
            // substitution-cipher scramble ("the" -> "bis"). Fold the resolved
            // base encoding name and the /Differences [code -> glyph name] pairs
            // so such fonts get distinct keys, while genuinely identical
            // encodings still dedup.
            if let Some(enc) = d.get("Encoding") {
                if let Some(enc_obj) = self.resolve_indirect_for_hash(enc) {
                    if let Some(enc_dict) = enc_obj.as_dict() {
                        20u8.hash(&mut hasher);
                        if let Some(Object::Name(base)) = enc_dict.get("BaseEncoding") {
                            base.hash(&mut hasher);
                        }
                        if let Some(diffs) = enc_dict.get("Differences") {
                            if let Some(diffs_obj) = self.resolve_indirect_for_hash(diffs) {
                                if let Some(arr) = diffs_obj.as_array() {
                                    for item in arr {
                                        match item {
                                            Object::Integer(i) => i.hash(&mut hasher),
                                            Object::Name(n) => n.hash(&mut hasher),
                                            Object::Reference(r) => {
                                                r.id.hash(&mut hasher);
                                                r.gen.hash(&mut hasher);
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Simple fonts (Type1/TrueType) carry their embedded program on the
            // top-level /FontDescriptor. Two subset fonts that share a
            // /BaseFont name but embed different glyph programs must not alias.
            if let Some(fd) = d.get("FontDescriptor") {
                if let Some(fd_obj) = self.resolve_indirect_for_hash(fd) {
                    self.fold_font_program(&fd_obj, 18, &mut hasher);
                }
            }

            if let Some(Object::Array(arr)) = d.get("DescendantFonts") {
                // Domain separator for the descendant section.
                11u8.hash(&mut hasher);
                for item in arr {
                    let resolved = match item {
                        Object::Reference(r) => self.load_object(*r).ok(),
                        Object::Dictionary(_) => Some(item.clone()),
                        _ => None,
                    };
                    let desc = match resolved {
                        Some(d) => d,
                        None => continue,
                    };
                    let dd = match desc.as_dict() {
                        Some(dd) => dd,
                        None => continue,
                    };

                    // /DW — default horizontal width on the CIDFont. Always
                    // int in well-formed PDFs; we accept Real defensively.
                    if let Some(dw) = dd.get("DW") {
                        12u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(dw, &mut hasher);
                    }
                    // /DW2 — default vertical metrics [v_y w1y]. Two-element
                    // numeric array per ISO 32000-1 §9.7.4.3.
                    if let Some(dw2) = dd.get("DW2") {
                        13u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(dw2, &mut hasher);
                    }
                    // /W — per-CID horizontal widths, may use form-a
                    // (c [w1 w2 …]) or form-b (c_first c_last w).
                    if let Some(w) = dd.get("W") {
                        14u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(w, &mut hasher);
                    }
                    // /W2 — per-CID vertical metrics, analogous to /W.
                    if let Some(w2) = dd.get("W2") {
                        15u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(w2, &mut hasher);
                    }
                    // /CIDSystemInfo — folded so otherwise-identical dicts
                    // targeting different registries don't collide.
                    if let Some(csi) = dd.get("CIDSystemInfo") {
                        16u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(csi, &mut hasher);
                    }
                    // Descendant /Subtype: CIDFontType0 (CFF) and CIDFontType2
                    // (TrueType) are not interchangeable even with identical
                    // name + metrics; the top-level Subtype is `Type0` for both.
                    if let Some(st) = dd.get("Subtype") {
                        19u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(st, &mut hasher);
                    }
                    // Embedded CIDFont program lives on the descendant's
                    // /FontDescriptor (/FontFile2 for TrueType, /FontFile3 for
                    // CFF). Folded under a distinct section so it cannot alias
                    // a simple font's top-level program.
                    if let Some(fd) = dd.get("FontDescriptor") {
                        if let Some(fd_obj) = self.resolve_indirect_for_hash(fd) {
                            self.fold_font_program(&fd_obj, 20, &mut hasher);
                        }
                    }
                    // Descendant /CIDToGIDMap: the *stream* form remaps
                    // CID→glyph (§9.7.4.3), so two otherwise-identical embedded
                    // CIDFontType2 fonts with different maps select different
                    // glyphs and must not alias. The `/Identity` name — and an
                    // absent entry, which defaults to Identity — fold nothing,
                    // so the common path's key is unchanged (and an explicit
                    // `/Identity` still dedups with an absent one).
                    if let Some(c2g) = dd.get("CIDToGIDMap") {
                        if !matches!(c2g, Object::Name(_)) {
                            21u8.hash(&mut hasher);
                            self.fold_stream_bytes(c2g, &mut hasher);
                        }
                    }
                }
            }
        }

        hasher.finish()
    }

    /// Resolve a single level of indirection for hashing: returns the
    /// referenced object, the object itself when already inline, or `None`
    /// when a reference cannot be loaded (cycle/missing). Used only to reach a
    /// `/FontDescriptor` dict — it never re-enters the font dict, so it cannot
    /// loop.
    fn resolve_indirect_for_hash(&self, obj: &Object) -> Option<Object> {
        match obj {
            Object::Reference(r) => self.load_object(*r).ok(),
            other => Some(other.clone()),
        }
    }

    /// Fold the *raw* bytes of a (possibly indirectly-referenced) stream into
    /// the hash. Folds nothing when the object is absent, unreadable, or not a
    /// stream.
    ///
    /// Raw — still-encoded — bytes are deliberate. They are a sufficient
    /// discriminator: different decoded content yields different encoded bytes
    /// under any deterministic filter, so this never produces a *false* dedup
    /// (two different fonts sharing a key). It avoids inflating large font
    /// programs on the cache-key path. The only cost is a *missed* dedup when
    /// the same logical content is stored under two different filters
    /// (e.g. raw vs. FlateDecode) — harmless, and not a pattern a single
    /// producer emits within a corpus.
    fn fold_stream_bytes<H: std::hash::Hasher>(&self, obj: &Object, hasher: &mut H) {
        use std::hash::Hash;
        let owned;
        let stream: &Object = match obj {
            Object::Stream { .. } => obj,
            Object::Reference(r) => match self.load_object(*r) {
                Ok(o) => {
                    owned = o;
                    &owned
                }
                Err(_) => return,
            },
            _ => return,
        };
        if let Object::Stream { data, .. } = stream {
            (data.len() as u64).hash(hasher);
            data.as_ref().hash(hasher);
        }
    }

    /// Fold any embedded font program (`/FontFile`, `/FontFile2`,
    /// `/FontFile3`) reachable from a `/FontDescriptor` dict into the hash,
    /// namespaced by `section` so a simple font's program and a descendant
    /// CIDFont's program cannot alias each other.
    fn fold_font_program<H: std::hash::Hasher>(
        &self,
        descriptor: &Object,
        section: u8,
        hasher: &mut H,
    ) {
        use std::hash::Hash;
        let dict = match descriptor.as_dict() {
            Some(d) => d,
            None => return,
        };
        for (variant, key) in ["FontFile", "FontFile2", "FontFile3"].iter().enumerate() {
            if let Some(ff) = dict.get(*key) {
                section.hash(hasher);
                (variant as u8).hash(hasher);
                self.fold_stream_bytes(ff, hasher);
            }
        }
    }

    /// Hash a PDF `Object` deterministically. Used by the descendant-aware
    /// font identity hash to fold raw width-array content into the key.
    ///
    /// Cycles are not possible for /W, /W2, /DW2 or /CIDSystemInfo content
    /// in any conformant PDF: these are pure data subtrees (numbers,
    /// arrays of numbers, occasional name/integer dicts), never indirect
    /// references back to a font dict. We still avoid recursing into
    /// streams (whose data we deliberately exclude from the cheap hash)
    /// and into unresolved references (we hash the ref's id/gen, not the
    /// pointed-to bytes — the per-font cache key already covers the
    /// referenced descendant CIDFont).
    fn hash_pdf_object_deterministic<H: std::hash::Hasher>(obj: &Object, hasher: &mut H) {
        use std::hash::Hash;
        match obj {
            Object::Null => 0u8.hash(hasher),
            Object::Boolean(b) => {
                1u8.hash(hasher);
                b.hash(hasher);
            }
            Object::Integer(i) => {
                2u8.hash(hasher);
                i.hash(hasher);
            }
            // Bit-pattern hash so two equal values hash identically without
            // tripping over f64's missing `Hash` impl. NaN is not produced
            // by PDF parsers from numeric tokens.
            Object::Real(r) => {
                3u8.hash(hasher);
                r.to_bits().hash(hasher);
            }
            Object::String(s) => {
                4u8.hash(hasher);
                s.hash(hasher);
            }
            Object::Name(n) => {
                5u8.hash(hasher);
                n.hash(hasher);
            }
            Object::Array(arr) => {
                6u8.hash(hasher);
                (arr.len() as u64).hash(hasher);
                for item in arr {
                    Self::hash_pdf_object_deterministic(item, hasher);
                }
            }
            Object::Dictionary(d) => {
                7u8.hash(hasher);
                // Sort keys for deterministic ordering — HashMap iteration
                // is randomized per process.
                let mut keys: Vec<&str> = d.keys().map(|k| k.as_str()).collect();
                keys.sort_unstable();
                (keys.len() as u64).hash(hasher);
                for k in keys {
                    k.hash(hasher);
                    if let Some(v) = d.get(k) {
                        Self::hash_pdf_object_deterministic(v, hasher);
                    }
                }
            }
            Object::Reference(r) => {
                8u8.hash(hasher);
                r.id.hash(hasher);
                r.gen.hash(hasher);
            }
            // Streams: dict shape only; we do not pull stream data into
            // the font identity hash (kept consistent with the cheap path).
            Object::Stream { dict, .. } => {
                9u8.hash(hasher);
                let mut keys: Vec<&str> = dict.keys().map(|k| k.as_str()).collect();
                keys.sort_unstable();
                (keys.len() as u64).hash(hasher);
                for k in keys {
                    k.hash(hasher);
                    if let Some(v) = dict.get(k) {
                        Self::hash_pdf_object_deterministic(v, hasher);
                    }
                }
            }
        }
    }

    fn font_identity_hash_cheap(font_obj: &Object) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();

        if let Some(d) = font_obj.as_dict() {
            // BaseFont: primary identity — unique per font within a document
            if let Some(Object::Name(n)) = d.get("BaseFont") {
                1u8.hash(&mut hasher);
                n.hash(&mut hasher);
            }
            // Subtype: Type1, TrueType, Type0, CIDFontType0, CIDFontType2
            if let Some(Object::Name(n)) = d.get("Subtype") {
                2u8.hash(&mut hasher);
                n.hash(&mut hasher);
            }
            // Encoding: hash inline name or presence of reference
            if let Some(enc) = d.get("Encoding") {
                3u8.hash(&mut hasher);
                match enc {
                    Object::Name(n) => n.hash(&mut hasher),
                    Object::Reference(_) => b"enc_ref".hash(&mut hasher),
                    Object::Dictionary(_) => b"enc_dict".hash(&mut hasher),
                    _ => {}
                }
            }
            // ToUnicode: hash content via reference or inline presence
            if let Some(to_unicode) = d.get("ToUnicode") {
                4u8.hash(&mut hasher);
                if let Some(r) = to_unicode.as_reference() {
                    r.id.hash(&mut hasher);
                    r.gen.hash(&mut hasher);
                }
            }
            // FontDescriptor: hash presence
            if d.get("FontDescriptor").is_some() {
                5u8.hash(&mut hasher);
            }
            // DescendantFonts: hash references for Type0 fonts
            if let Some(Object::Array(arr)) = d.get("DescendantFonts") {
                6u8.hash(&mut hasher);
                for item in arr {
                    if let Some(r) = item.as_reference() {
                        r.id.hash(&mut hasher);
                        r.gen.hash(&mut hasher);
                    }
                }
            }
            // #598: width metrics. Two non-subset fonts can share
            // BaseFont + Subtype + Encoding yet ship different glyph widths —
            // Standard-14 fonts may carry producer-specific /Widths overrides
            // (§9.6.2.2), and differently-optimized embeds of the same named
            // font diverge similarly. Without folding widths into the key,
            // such fonts collide on the cross-document cache and the second
            // document gets the first's advances. We hash the simple-font
            // char range + width table and the Type0 default width. Only
            // values present inline on this dict are reachable (this is a pure
            // function over the font object); a referenced /Widths or the
            // descendant CIDFont /W array falls back to the coarser key — an
            // accepted, documented limitation, not a new regression.
            if let Some(Object::Integer(first_char)) = d.get("FirstChar") {
                7u8.hash(&mut hasher);
                first_char.hash(&mut hasher);
            }
            if let Some(Object::Integer(last_char)) = d.get("LastChar") {
                8u8.hash(&mut hasher);
                last_char.hash(&mut hasher);
            }
            if let Some(Object::Array(widths)) = d.get("Widths") {
                9u8.hash(&mut hasher);
                (widths.len() as u64).hash(&mut hasher);
                for w in widths {
                    match w {
                        Object::Integer(i) => i.hash(&mut hasher),
                        // Bit-pattern hash so equal widths hash equally
                        // (these are glyph advances, never NaN in practice).
                        Object::Real(r) => r.to_bits().hash(&mut hasher),
                        _ => 0u8.hash(&mut hasher),
                    }
                }
            }
            // Type0 default width, when present inline on the font dict.
            if let Some(Object::Integer(dw)) = d.get("DW") {
                10u8.hash(&mut hasher);
                dw.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// Whether a font dictionary describes a font that is *document-local* and
    /// therefore must never be served from / inserted into the cross-document
    /// global font cache (Layer 6), even if its cheap identity hash collides
    /// with a font in another document.
    ///
    /// Type 3 fonts (PDF 32000-1 §9.6.5) define their glyphs as streams of PDF
    /// graphics operators in a `/CharProcs` dictionary whose procedures
    /// reference the *owning document's* resources (XObjects, ColorSpaces,
    /// ExtGState, …). Two Type 3 fonts from different documents that happen to
    /// share `/Name` + `/Encoding` shape are NOT interchangeable: serving one
    /// document's parsed `FontInfo` for the other yields wrong glyphs. Such
    /// fonts carry no subset prefix, so the cheap hash cannot distinguish them
    /// — this predicate gates them out of the global cache instead (#597).
    fn font_is_document_local(font_obj: &Object) -> bool {
        let dict = match font_obj.as_dict() {
            Some(d) => d,
            None => return false,
        };

        // Type 3 fonts reference this document's resources via their CharProcs,
        // so a cached FontInfo cannot cross PdfDocument boundaries.
        if dict.get("Subtype").and_then(|s| s.as_name()) == Some("Type3") {
            return true;
        }

        // Subset fonts carry a document-specific glyph subset and ToUnicode
        // CMap, so they are unsafe to share across documents even when the
        // BaseFont name collides. A subset BaseFont is tagged with exactly six
        // uppercase letters and a '+' per ISO 32000-1:2008 §9.6.4
        // (e.g. `AAAAAA+ArialUnicodeMS`).
        match dict.get("BaseFont").and_then(|b| b.as_name()) {
            Some(base_font) => Self::is_subset_basefont(base_font),
            // A non-Type3 font is required by the spec to carry /BaseFont; if it
            // is absent we cannot prove the font is shareable, so fail safe and
            // treat it as document-local rather than risk poisoning the cache.
            None => true,
        }
    }

    /// Detect a PDF subset-font tag on a `/BaseFont` name: exactly six uppercase
    /// ASCII letters followed by `+`, per ISO 32000-1:2008 §9.6.4 (e.g.
    /// `AAAAAA+ArialUnicodeMS`). `is_ascii_uppercase` is precisely A–Z, so
    /// multibyte (CJK) names never satisfy the test and are treated as full
    /// fonts — correct, since subset tags are by definition ASCII A–Z.
    fn is_subset_basefont(base_font: &str) -> bool {
        let bytes = base_font.as_bytes();
        bytes.len() > 7 && bytes[6] == b'+' && bytes[..6].iter().all(|b| b.is_ascii_uppercase())
    }

    /// Load fonts from a Resources dictionary into the extractor.
    pub(crate) fn load_fonts(
        &self,
        resources: &Object,
        extractor: &mut crate::extractors::TextExtractor<'_>,
    ) -> Result<()> {
        use crate::fonts::FontInfo;

        // Resources can be a reference or a dictionary
        let resources_obj = if let Some(res_ref) = resources.as_reference() {
            self.load_object(res_ref)?
        } else {
            resources.clone()
        };

        let resources_dict = match resources_obj.as_dict() {
            Some(d) => d,
            None => {
                log::warn!(
                    "Resources is not a dictionary (type: {}), treating as empty",
                    resources_obj.type_name()
                );
                return Ok(());
            }
        };

        // Get Font dictionary if present
        if let Some(font_obj) = resources_dict.get("Font") {
            // Font can be a reference or direct dictionary - need to dereference
            let font_dict_ref = font_obj.as_reference();
            let font_dict_obj = if let Some(font_ref) = font_dict_ref {
                self.load_object(font_ref)?
            } else {
                font_obj.clone()
            };

            // Layer 2: Check font set cache for the /Font dictionary.
            // Pages sharing the same /Font dict skip the entire per-font loop.
            if let Some(font_dict_ref) = font_dict_ref {
                let cached_set_opt = self
                    .font_set_cache
                    .lock_or_recover()
                    .get(&font_dict_ref)
                    .cloned();
                if let Some(cached_set) = cached_set_opt {
                    for (name, font_arc) in &cached_set {
                        extractor.add_font_shared(name.clone(), Arc::clone(font_arc));
                    }
                    extractor.share_truetype_cmaps();
                    return Ok(());
                }
            }

            if let Some(font_dict) = font_dict_obj.as_dict() {
                // Compute font fingerprint from (name → ObjectRef) pairs.
                // Hash the MAPPING between font names and their object refs,
                // not just the sets separately. This prevents false cache hits
                // when two font dicts have the same set of refs and names but
                // different name-to-ref assignments.
                let fingerprint = {
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    let mut name_ref_pairs: Vec<(&str, Option<ObjectRef>)> = font_dict
                        .iter()
                        .map(|(name, fo)| (name.as_str(), fo.as_reference()))
                        .collect();
                    name_ref_pairs.sort_by(|a, b| a.0.cmp(b.0));
                    for (name, obj_ref) in &name_ref_pairs {
                        name.hash(&mut hasher);
                        if let Some(r) = obj_ref {
                            r.id.hash(&mut hasher);
                            r.gen.hash(&mut hasher);
                        }
                    }
                    hasher.finish()
                };

                let cached_fingerprint_opt = self
                    .font_fingerprint_cache
                    .lock_or_recover()
                    .get(&fingerprint)
                    .cloned();
                if let Some(cached_set) = cached_fingerprint_opt {
                    for (name, font_arc) in &cached_set {
                        extractor.add_font_shared(name.clone(), Arc::clone(font_arc));
                    }
                    extractor.share_truetype_cmaps();
                    return Ok(());
                }

                // Layer 4: Name-based font set cache with spot-check verification.
                // Pages in the same document often use the same font names mapped to
                // different ObjectRefs but identical base fonts (e.g., 764 pages each
                // creating T1_0→Helvetica, T1_1→Times-Roman with unique object numbers).
                // Cache the resolved font set by sorted font names, then on subsequent
                // pages verify ONE font via load+hash to confirm the mapping is the same.
                let name_hash = {
                    use std::hash::{Hash, Hasher};
                    let mut font_names: Vec<&str> = font_dict.keys().map(|k| k.as_str()).collect();
                    font_names.sort();
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    font_names.hash(&mut hasher);
                    hasher.finish()
                };

                let cached_name_set = self
                    .font_name_set_cache
                    .lock_or_recover()
                    .get(&name_hash)
                    .cloned();
                // Sort font entries by name for deterministic processing order.
                // HashMap iteration order is randomized per-process, which causes
                // non-deterministic text extraction when font CMap sharing depends
                // on the order fonts are loaded.
                let mut sorted_font_entries: Vec<(&String, &Object)> = font_dict.iter().collect();
                sorted_font_entries.sort_by_key(|(name, _)| name.as_str());

                if let Some((cached_set, check_hash)) = cached_name_set {
                    // Verify the cached font set by computing a combined identity hash
                    // over ALL reference fonts in the current Resources dict (sorted by
                    // name). This prevents false cache hits when pages reuse the same
                    // font key names but embed different per-page subsets — a single-font
                    // spot-check is insufficient because it only guards one entry
                    // lets differing sibling fonts (F2, F3 …) slip through unchecked.
                    // Fixes the regression described in issue #408.
                    let current_combined = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        for (name, font_obj) in &sorted_font_entries {
                            if let Some(font_ref) = font_obj.as_reference() {
                                if let Some(fh) = self.cached_font_identity_hash(font_ref) {
                                    name.as_str().hash(&mut h);
                                    fh.hash(&mut h);
                                }
                            }
                        }
                        h.finish()
                    };
                    if current_combined == check_hash {
                        for (name, font_arc) in cached_set.iter() {
                            extractor.add_font_shared(name.clone(), Arc::clone(font_arc));
                        }
                        extractor.share_truetype_cmaps();
                        return Ok(());
                    }
                    // Hash mismatch: fonts differ — fall through to full load.
                }

                // Snapshot names already in the extractor before this load_fonts call.
                // Layer 4 must store only the delta so that a cache hit never injects
                // parent-page fonts into a different page's extractor context, which
                // would overwrite correctly-loaded fonts with wrong versions.
                let extractor_names_before: std::collections::HashSet<String> = extractor
                    .get_font_set()
                    .into_iter()
                    .map(|(k, _)| k)
                    .collect();

                let mut all_from_cache = true;

                for (name, font_obj) in &sorted_font_entries {
                    // If font is a reference, check per-font cache first
                    if let Some(font_ref) = font_obj.as_reference() {
                        let cached_font_opt =
                            self.font_cache.lock_or_recover().get(&font_ref).cloned();
                        if let Some(cached) = cached_font_opt {
                            extractor.add_font_shared((*name).clone(), cached);
                            continue;
                        }
                        all_from_cache = false;
                        let font = self.load_object(font_ref)?;

                        // Compute identity hash. For Type0 fonts this also
                        // resolves the descendant CIDFont and folds its
                        // /DW, /DW2, /W, /W2 into the key — otherwise two
                        // Type0 fonts whose top-level dicts have identical
                        // inline shape but whose CIDFonts ship different
                        // horizontal or vertical metrics would collide on
                        // the Layer 5/6 caches.
                        let id_hash = self.font_identity_hash_with_descendants(&font);

                        // Type 3 fonts and subset fonts must not cross
                        // PdfDocument boundaries via the global cache — their
                        // glyph procs / glyph-subset + ToUnicode mappings are
                        // document-specific. The per-document Layer 4/5 caches
                        // below stay safe to use.
                        let is_document_local = Self::font_is_document_local(&font);

                        // Layer 5: Per-font identity cache — skip from_dict when a
                        // structurally identical font was already parsed elsewhere.
                        let cached_identity_opt = self
                            .font_identity_cache
                            .lock_or_recover()
                            .get(&id_hash)
                            .cloned();
                        if let Some(cached) = cached_identity_opt {
                            self.font_cache
                                .lock_or_recover()
                                .insert(font_ref, Arc::clone(&cached));
                            extractor.add_font_shared((*name).clone(), cached);
                            continue;
                        }

                        // Layer 6: Global cross-document font cache — reuse fonts
                        // parsed by previous PdfDocument instances in this process.
                        // Skipped entirely for document-local fonts (#597).
                        if !is_document_local {
                            if let Some(cached) =
                                crate::fonts::global_cache::global_font_cache_get(id_hash)
                            {
                                self.font_identity_cache
                                    .lock_or_recover()
                                    .insert(id_hash, Arc::clone(&cached));
                                self.font_cache
                                    .lock_or_recover()
                                    .insert(font_ref, Arc::clone(&cached));
                                extractor.add_font_shared((*name).clone(), cached);
                                continue;
                            }
                        }

                        match FontInfo::from_dict(&font, self) {
                            Ok(font_info) => {
                                let arc = Arc::new(font_info);
                                // Populate the document-level caches always; the
                                // global cross-document cache only for fonts that
                                // are safe to share across documents (#597).
                                if !is_document_local {
                                    crate::fonts::global_cache::global_font_cache_insert(
                                        id_hash,
                                        Arc::clone(&arc),
                                    );
                                }
                                self.font_identity_cache
                                    .lock_or_recover()
                                    .insert(id_hash, Arc::clone(&arc));
                                self.font_cache
                                    .lock_or_recover()
                                    .insert(font_ref, Arc::clone(&arc));
                                extractor.add_font_shared((*name).clone(), arc);
                            }
                            Err(e) => {
                                log::error!(
                                    "Failed to load font '{}': {}. Text using this font will use fallback encoding.",
                                    name,
                                    e
                                );
                                continue;
                            }
                        }
                    } else {
                        // Direct font object — parse without caching (no stable key)
                        all_from_cache = false;
                        let font = *font_obj;
                        match FontInfo::from_dict(font, self) {
                            Ok(font_info) => {
                                extractor.add_font((*name).clone(), font_info);
                            }
                            Err(e) => {
                                log::error!(
                                    "Failed to load font '{}': {}. Text using this font will use fallback encoding.",
                                    name,
                                    e
                                );
                                continue;
                            }
                        }
                    }
                }

                // Always re-share TrueType CMaps after loading fonts. Cached fonts
                // may lack donated CMaps because Arc::make_mut creates a per-extractor
                // clone that is not written back to per-font cache. A donor font added
                // in a later load_fonts call (e.g. an XObject font donating to a
                // page-level font already in the extractor) requires sharing to run
                // again even when all fonts came from cache.
                extractor.share_truetype_cmaps();

                // Cache font set by both ObjectRef and fingerprint
                let font_set = extractor.get_font_set();
                if let Some(fdr) = font_dict_ref {
                    self.font_set_cache
                        .lock_or_recover()
                        .insert(fdr, font_set.clone());
                }
                self.font_fingerprint_cache
                    .lock_or_recover()
                    .insert(fingerprint, font_set.clone());

                // Cache by font names for Layer 4. Store only the delta — fonts
                // added by THIS load_fonts call — so that a cache hit never pollutes
                // a different page's extractor with stale parent-page fonts.
                // The combined identity hash covers ALL reference fonts (sorted by
                // name), so a hit requires every font in the Resources dict to match,
                // not just one. This prevents false positives when pages reuse the
                // same font key names with different per-page subsets.
                if !all_from_cache {
                    let combined_check_hash = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        for (name, font_obj) in &sorted_font_entries {
                            if let Some(font_ref) = font_obj.as_reference() {
                                if let Some(fh) = self.cached_font_identity_hash(font_ref) {
                                    name.as_str().hash(&mut h);
                                    fh.hash(&mut h);
                                }
                            }
                        }
                        h.finish()
                    };
                    let l4_set: Vec<(String, Arc<FontInfo>)> = font_set
                        .iter()
                        .filter(|(k, _)| !extractor_names_before.contains(k.as_str()))
                        .map(|(k, v)| (k.clone(), Arc::clone(v)))
                        .collect();
                    self.font_name_set_cache
                        .lock_or_recover()
                        .insert(name_hash, (Arc::new(l4_set), combined_check_hash));
                }

                return Ok(());
            }
        }

        Ok(())
    }

    /// Extract tables from a page using structure tree and spatial detection.
    ///
    /// Tries two strategies in order:
    /// 1. **Structure tree** (tagged PDFs): Finds Table elements in the structure
    ///    tree and extracts cell content via MCID matching.
    /// 2. **Spatial detection** (untagged PDFs): Uses X/Y coordinate clustering
    ///    to detect grid-aligned text as tables.
    ///
    /// Returns early with structure tree tables if found (high confidence).
    fn extract_page_tables(
        &self,
        page_index: usize,
        spans: &[TextSpan],
        options: &crate::converters::ConversionOptions,
        text_fallback: bool,
    ) -> Vec<crate::structure::Table> {
        // Strategy 1: Structure tree (tagged PDFs)
        let struct_tree_opt = {
            let cached = self.structure_tree_cache.lock_or_recover().clone();
            match cached {
                Some(tree) => tree,
                None => {
                    let is_marked = self.mark_info().map(|m| m.marked).unwrap_or(false);
                    let has_struct_tree_root = !is_marked
                        && self
                            .catalog()
                            .ok()
                            .and_then(|cat| cat.as_dict().map(|d| d.contains_key("StructTreeRoot")))
                            .unwrap_or(false);
                    let tree = if is_marked || has_struct_tree_root {
                        self.structure_tree().ok().flatten().map(Arc::new)
                    } else {
                        None
                    };
                    *self.structure_tree_cache.lock_or_recover() = Some(tree.clone());
                    tree
                }
            }
        };
        if let Some(ref struct_tree) = struct_tree_opt {
            // Build the per-page Table-element buckets once, then look up.
            if self.table_elements_cache.lock_or_recover().is_none() {
                let all = crate::structure::find_table_elements_all_pages(struct_tree);
                *self.table_elements_cache.lock_or_recover() = Some(all);
            }
            let table_elems: Vec<crate::structure::StructElem> = self
                .table_elements_cache
                .lock_or_recover()
                .as_ref()
                .and_then(|c| c.get(&(page_index as u32)))
                .cloned()
                .unwrap_or_default();
            if !table_elems.is_empty() {
                let mut tables = Vec::new();
                for table_elem in &table_elems {
                    match crate::structure::extract_table_from_spans(table_elem, spans) {
                        Ok(mut table) if !table.is_empty() => {
                            // Compute bbox from spans matching the table's MCIDs
                            if table.bbox.is_none() {
                                let all_mcids: HashSet<u32> = table
                                    .rows
                                    .iter()
                                    .flat_map(|r| {
                                        r.cells.iter().flat_map(|c| c.mcids.iter().copied())
                                    })
                                    .collect();
                                if !all_mcids.is_empty() {
                                    let mut min_x = f32::INFINITY;
                                    let mut min_y = f32::INFINITY;
                                    let mut max_x = f32::NEG_INFINITY;
                                    let mut max_y = f32::NEG_INFINITY;
                                    for span in spans {
                                        if let Some(mcid) = span.mcid {
                                            if all_mcids.contains(&mcid) {
                                                min_x = min_x.min(span.bbox.x);
                                                min_y = min_y.min(span.bbox.y);
                                                max_x = max_x.max(span.bbox.x + span.bbox.width);
                                                max_y = max_y.max(span.bbox.y + span.bbox.height);
                                            }
                                        }
                                    }
                                    if min_x < max_x && min_y < max_y {
                                        table.bbox = Some(crate::geometry::Rect::new(
                                            min_x,
                                            min_y,
                                            max_x - min_x,
                                            max_y - min_y,
                                        ));
                                    }
                                }
                            }
                            tables.push(table);
                        }
                        _ => {}
                    }
                }
                if !tables.is_empty() {
                    log::debug!(
                        "Found {} table(s) via structure tree for page {}",
                        tables.len(),
                        page_index
                    );
                    return tables;
                }
            }
        }

        // Strategy 2: Hybrid spatial detection (v0.3.14)
        let mut config = options.table_detection_config.clone().unwrap_or_default();
        // Honour the caller's text_fallback choice regardless of the default
        // on `TableDetectionConfig` — `extract_text` / `to_plain_text` pass
        // `text_fallback=false` to opt out of text-only spatial fallback even
        // though the type-level default is `true`.
        config.text_fallback = text_fallback;

        // Extract vector paths (lines/rects) for visual detection
        let paths = self.extract_paths(page_index).unwrap_or_default();

        // Filter to table-relevant paths (lines and rectangles only).
        // Chart/plot pages often have hundreds of curves and fills that
        // extract_edges ignores anyway — passing them through the full
        // detection pipeline wastes O(n²) time.
        const LINE_TOL: f32 = 2.0;
        let table_paths: Vec<_> = paths
            .into_iter()
            .filter(|p| {
                p.is_horizontal_line(LINE_TOL) || p.is_vertical_line(LINE_TOL) || p.is_rectangle()
            })
            .collect();

        // A page with thousands of line/rect paths is a drawing or chart, not a
        // ruled table; skip the O(E²) collinear-join + intersection sweep. Real
        // ruled tables have at most a few hundred edges. (Tagged tables already
        // returned above via the structure tree.)
        const MAX_TABLE_EDGES: usize = 1500;
        if table_paths.len() > MAX_TABLE_EDGES {
            log::debug!(
                "Page {} has {} line/rect paths (> {}) — skipping spatial table sweep",
                page_index,
                table_paths.len(),
                MAX_TABLE_EDGES
            );
            return Vec::new();
        }

        if table_paths.is_empty() {
            use crate::structure::spatial_table_detector::TableStrategy;
            let is_text_only = matches!(
                (config.horizontal_strategy, config.vertical_strategy),
                (TableStrategy::Text, TableStrategy::Text)
            );
            if !is_text_only && !config.text_fallback {
                return Vec::new();
            }
            if !is_text_only && config.text_fallback {
                log::debug!(
                    "No ruling lines on page {} — using text-only spatial fallback (issue #486)",
                    page_index
                );
            }
        }
        let paths = table_paths;

        let words = self.extract_words(page_index).unwrap_or_default();
        let word_spans: Vec<crate::layout::TextSpan> = words
            .into_iter()
            .map(|w| crate::layout::TextSpan {
                provenance: None,
                artifact_type: None,
                text: w.text,
                bbox: w.bbox,
                font_name: w.dominant_font,
                font_size: w.avg_font_size,
                font_weight: if w.is_bold {
                    crate::layout::FontWeight::Bold
                } else {
                    crate::layout::FontWeight::Normal
                },
                is_italic: w.is_italic,
                is_monospace: false,
                color: crate::layout::Color::black(),
                mcid: w.mcid,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 1.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                text_rise: 0.0,
                rtl_draw_logical: false,
            })
            .collect();

        // Fall back to raw spans if word extraction failed
        let input_spans = if !word_spans.is_empty() {
            &word_spans
        } else {
            spans
        };

        let raw_tables = crate::structure::spatial_table_detector::detect_tables_with_lines(
            input_spans,
            &paths,
            &config,
        );

        // Issue 484/486/487: when a logical multi-row table is drawn with a
        // horizontal ruling line between every pair of rows, the line-based
        // detector emits one Table per row strip. Each fragment is a 1- or
        // 2-row table that fails is_real_grid below and gets dropped, after
        // which the cells fall through to paragraph flow with column-based
        // reading order — producing orphan `<p>40000≤Q</p>` /
        // `<p>＜55000</p>` pairs. Consolidate vertically-adjacent fragments
        // that share an identical column structure BEFORE applying
        // is_real_grid so the merged multi-row table survives the filter.
        let raw_tables =
            crate::structure::spatial_table_detector::consolidate_adjacent_table_fragments(
                raw_tables,
            );

        // Step 4: spatial detection without struct-tree backing
        // is prone to false positives on form-style layouts (label-colon-
        // value pairs that align horizontally, form fillable boxes drawn
        // with thin lines). Drop tables that don't look like real grids.
        let raw_count = raw_tables.len();
        let mut tables: Vec<crate::structure::Table> = raw_tables
            .into_iter()
            .filter(|t| t.is_real_grid())
            // Prose-shape filter — applies to line-based detection too: a
            // PDF with decorative horizontal rules (newsletter mastheads,
            // press-release banners) can hand `is_real_grid` a "wide data
            // table" that is actually wrapped paragraphs partitioned by
            // word x-alignment. Reject those before they reach the
            // converter. See `looks_like_prose_table` for the heuristic.
            .filter(|t| !looks_like_prose_table(t))
            .collect();

        if raw_count != tables.len() {
            log::debug!(
                "Spatial table detection: filtered {} non-real-grid candidates on page {} ({} kept)",
                raw_count - tables.len(),
                page_index,
                tables.len(),
            );
        } else if !tables.is_empty() {
            log::debug!(
                "Found {} table(s) via hybrid spatial detection for page {}",
                tables.len(),
                page_index
            );
        }

        // Text-only spatial fallback for converter paths (to_markdown / to_html — issue #486).
        //
        // Wide data tables (e.g. sailing-score grids with 16-18 columns) exceed the default
        // `max_table_columns: 15` limit and are rejected by the main pipeline. When the
        // caller explicitly opted in to text-only detection (text_fallback=true), retry with
        // a relaxed config that raises the column ceiling and adjusts tolerances so that
        // genuinely wide data tables are captured.
        //
        // Safety guards:
        // - Only fires when the main pipeline returned no tables (avoids double-counting).
        // - Only fires when the caller is a converter (text_fallback=true).
        // - Skipped for tagged PDFs: the structure tree already provides the authoritative
        //   layout; spatial heuristics produce false-positive tables from structure elements
        //   (e.g. headings detected as single-row tables — issue #486 regression).
        // - Skipped for predominantly-RTL pages: Arabic/Hebrew text alignment patterns
        //   mimic table columns in spatial heuristics — issue #486 regression.
        // - When ruling lines exist, spans are filtered to the line-bounded region to
        //   prevent page headers/footers from being erroneously included in the table.
        // - Results must pass is_real_grid() just like main-pipeline tables.

        // Guard 1 — Tagged PDFs: presence of a structure tree means the document has an
        // explicit semantic layout. Spatial text-only detection would misfire on
        // structure elements (headings, paragraphs) that happen to share a Y band.
        if config.text_fallback && struct_tree_opt.is_some() {
            log::debug!(
                "Text-only spatial fallback skipped for page {} — document has a structure tree (tagged PDF)",
                page_index
            );
            return tables;
        }

        // Guard 2 — RTL pages: Arabic and Hebrew text naturally aligns horizontally in
        // patterns that the column-clustering algorithm mistakes for table columns.
        // Skip spatial detection when more than 30 % of the input spans are RTL.
        if config.text_fallback {
            let rtl_count = input_spans
                .iter()
                .filter(|s| crate::text::bidi::looks_rtl(&s.text))
                .count();
            let rtl_fraction = rtl_count as f32 / input_spans.len().max(1) as f32;
            if rtl_fraction > 0.30 {
                log::debug!(
                    "Text-only spatial fallback skipped for page {} — {:.0}% RTL spans (threshold 30%)",
                    page_index,
                    rtl_fraction * 100.0
                );
                return tables;
            }
        }

        if config.text_fallback && tables.is_empty() {
            use crate::structure::spatial_table_detector::detect_tables_from_spans_column_aware;
            // Build a relaxed config derived from the caller's config.
            // We only raise the limits known to block wide data tables (e.g. sailing
            // score grids with 16-18 columns that exceed the default max_table_columns=15).
            let relaxed_config = crate::structure::spatial_table_detector::TableDetectionConfig {
                // Allow up to 25 columns — covers 17-column sailing score tables.
                max_table_columns: config.max_table_columns.max(25),
                // Tighter column grouping than the default 15 pt so that nearby
                // score columns are not merged into each other.
                column_tolerance: config.column_tolerance.min(10.0),
                // Looser merge threshold so that columns with slight X scatter
                // (e.g. centred numeric cells) are aggregated correctly.
                column_merge_threshold: config.column_merge_threshold.max(30.0),
                // Inherit all other settings from caller's config.
                ..config.clone()
            };

            // When ruling lines are present on the page, restrict text detection to
            // spans that fall within the VERTICAL-LINE Y bounds. Vertical lines
            // define the table's column structure and their Y extent precisely
            // delineates the table rows, excluding page headers and footers which
            // sit above/below the table frame.
            //
            // Note: we use V-line Y bounds specifically (not total path bbox) because
            // H-lines in these PDFs often span the full page height (outer frame),
            // while V-lines are confined to the interior table region.
            let candidate_spans: Vec<crate::layout::TextSpan>;
            let fallback_spans: &[crate::layout::TextSpan] = {
                let v_lines: Vec<_> = paths.iter().filter(|p| p.is_vertical_line(2.0)).collect();
                if !v_lines.is_empty() {
                    // Rendered extents: a stroke-width-encoded column rule's
                    // drawn bar spans the table height while its geometric
                    // bbox is a ~0pt speck at the midline —
                    // banding on the speck would filter out the table's own
                    // spans.
                    let vline_y_min = v_lines
                        .iter()
                        .map(|p| p.rendered_bbox().y)
                        .fold(f32::INFINITY, f32::min);
                    let vline_y_max = v_lines
                        .iter()
                        .map(|p| {
                            let r = p.rendered_bbox();
                            r.y + r.height
                        })
                        .fold(f32::NEG_INFINITY, f32::max);
                    // Small margin to include spans whose centres just touch the frame.
                    const V_MARGIN: f32 = 5.0;
                    candidate_spans = input_spans
                        .iter()
                        .filter(|s| {
                            let cy = s.bbox.y + s.bbox.height * 0.5;
                            cy >= vline_y_min - V_MARGIN && cy <= vline_y_max + V_MARGIN
                        })
                        .cloned()
                        .collect();
                    log::debug!(
                        "Text fallback (page {}): V-lines Y=[{:.1},{:.1}] — filtered {} spans to {}",
                        page_index,
                        vline_y_min,
                        vline_y_max,
                        input_spans.len(),
                        candidate_spans.len()
                    );
                    &candidate_spans
                } else {
                    input_spans
                }
            };

            let text_candidates =
                detect_tables_from_spans_column_aware(fallback_spans, &relaxed_config);
            let pre_filter = text_candidates.len();
            let text_tables: Vec<_> = text_candidates
                .into_iter()
                // Text-only detection infers columns from word x-alignment
                // alone; a title + a wrapped body line (two rows) is the
                // signature of ordinary prose, not a table. Require ≥3
                // rows of evidence before promoting to a table.
                .filter(|t| t.rows.len() >= 3 && t.is_real_grid())
                // Prose split across many "columns" is the dominant
                // false-positive shape for text-only detection on
                // line-less pages: a paragraph wraps to N lines, words
                // cluster into N×K cells, and `is_real_grid` accepts the
                // shape. Real data-table cells almost never end with a
                // comma or semicolon (those punctuation marks belong to
                // running sentences), so a high comma-tail ratio is the
                // most discriminating prose signal we have.
                .filter(|t| !looks_like_prose_table(t))
                .collect();
            if !text_tables.is_empty() {
                log::debug!(
                    "Text-only relaxed fallback found {} table(s) on page {} ({} filtered by is_real_grid) — issue #486",
                    text_tables.len(),
                    page_index,
                    pre_filter - text_tables.len(),
                );
                tables = text_tables;
            }
        }

        tables
    }

    /// Extract images from a page.
    ///
    /// Extracts all images from the specified page by processing the content stream.
    /// This includes:
    /// - Images referenced via `Do` operators (XObject calls)
    /// - Images in nested Form XObjects (with recursion)
    /// - Inline images (BI...ID...EI sequences)
    ///
    /// This method processes PDF content streams instead of only iterating the XObject
    /// dictionary. This ensures that images referenced via the `Do` operator in the content
    /// stream are properly extracted, including those in nested Form XObjects. ColorSpace
    /// indirect references are also resolved.
    ///
    /// Returns a vector of PdfImage objects representing the extracted images.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Returns
    ///
    /// A vector of PdfImage objects, one for each image found on the page.
    ///
    /// # Errors
    ///
    /// Returns an error if the page cannot be accessed or if image extraction fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let images = doc.extract_images(0)?;
    /// println!("Found {} images on page 1", images.len());
    /// for (i, image) in images.iter().enumerate() {
    ///     image.save_as_png(&format!("image_{}.png", i))?;
    /// }
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn extract_images(&self, page_index: usize) -> Result<Vec<crate::extractors::PdfImage>> {
        self.require_authenticated()?;
        self.extract_images_filtered(page_index, &ImageExtractFilter::default())
    }

    /// WS2.6: normalized top/bottom-band text lines that recur on a majority
    /// of pages — running headers/footers. Page-number digits are stripped so
    /// "Page 3"/"Page 4" collapse to one signature. Empty for documents under
    /// 3 pages (repetition can't be judged) or when nothing repeats.
    pub(crate) fn repeated_running_head_foot(
        &self,
        threshold: f32,
    ) -> std::collections::HashSet<String> {
        use std::collections::{HashMap, HashSet};
        let mut out: HashSet<String> = HashSet::new();
        let Ok(page_count) = self.page_count() else {
            return out;
        };
        if page_count < 3 {
            return out;
        }
        let min_occ = (((page_count as f32) * threshold).ceil() as usize).max(2);
        let mut occ: HashMap<String, usize> = HashMap::new();
        for p in 0..page_count {
            let media_h = self.get_page_media_box(p).map(|m| m.3).unwrap_or(792.0);
            let Ok(spans) = self.extract_spans(p) else {
                continue;
            };
            let mut seen: HashSet<String> = HashSet::new();
            for s in &spans {
                if !Self::in_head_foot_band(s, media_h) {
                    continue;
                }
                let norm = Self::normalize_band_line(&s.text);
                // Count each distinct line once per page.
                if norm.len() > 3 && seen.insert(norm.clone()) {
                    *occ.entry(norm).or_default() += 1;
                }
            }
        }
        for (text, n) in occ {
            if n >= min_occ {
                out.insert(text);
            }
        }
        out
    }

    /// True when a span sits in the top or bottom 15% band of the page.
    fn in_head_foot_band(s: &crate::layout::TextSpan, media_h: f32) -> bool {
        s.bbox.y > media_h * 0.85 || (s.bbox.y + s.bbox.height) < media_h * 0.15
    }

    /// Normalize a band line for repetition matching: drop ASCII digits (page
    /// numbers vary), collapse whitespace, lowercase.
    fn normalize_band_line(text: &str) -> String {
        let stripped: String = text.chars().filter(|c| !c.is_ascii_digit()).collect();
        stripped
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    /// True when `span` is a running header/footer to strip: in the top/bottom
    /// band and its normalized text is one of the `repeated` signatures.
    fn is_running_head_foot(
        span: &crate::layout::TextSpan,
        media_h: f32,
        repeated: &std::collections::HashSet<String>,
    ) -> bool {
        Self::in_head_foot_band(span, media_h)
            && repeated.contains(&Self::normalize_band_line(&span.text))
    }

    /// Extract embedded files / attachments (WS1.8a, ISO 32000-1 §7.11.4).
    ///
    /// Walks the catalog's `/Names /EmbeddedFiles` name tree and returns
    /// `(filename, decoded bytes)` for each `/Filespec`'s `/EF /F` (or `/UF`)
    /// embedded-file stream. Complements the existing embedded-file writer.
    /// Returns an empty vector when the document has no attachments.
    pub fn extract_embedded_files(&self) -> Result<Vec<(String, Vec<u8>)>> {
        self.require_authenticated()?;
        let catalog = self.catalog()?;
        let Some(cat_dict) = catalog.as_dict() else {
            return Ok(Vec::new());
        };
        // catalog → /Names → /EmbeddedFiles (name-tree root).
        let names = cat_dict
            .get("Names")
            .and_then(|n| self.resolve_object(n).ok());
        let Some(ef_root) = names
            .as_ref()
            .and_then(|n| n.as_dict())
            .and_then(|d| d.get("EmbeddedFiles"))
            .and_then(|e| self.resolve_object(e).ok())
        else {
            return Ok(Vec::new());
        };

        let mut filespecs: Vec<Object> = Vec::new();
        self.collect_embedded_filespecs(&ef_root, &mut filespecs, 0);

        let mut out = Vec::new();
        for fs in &filespecs {
            let Some(fs_dict) = fs.as_dict() else {
                continue;
            };
            // Prefer the Unicode filename /UF, fall back to /F.
            let filename = fs_dict
                .get("UF")
                .or_else(|| fs_dict.get("F"))
                .and_then(|n| self.resolve_object(n).ok())
                .and_then(|n| {
                    n.as_string()
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                })
                .unwrap_or_else(|| "attachment".to_string());
            // /EF → { /F <stream ref> } (or /UF).
            let Some(ef) = fs_dict.get("EF").and_then(|e| self.resolve_object(e).ok()) else {
                continue;
            };
            let Some(ef_dict) = ef.as_dict() else {
                continue;
            };
            let Some(stream_ref) = ef_dict
                .get("F")
                .or_else(|| ef_dict.get("UF"))
                .and_then(|r| r.as_reference())
            else {
                continue;
            };
            if let Ok(stream_obj) = self.load_object(stream_ref) {
                if let Ok(bytes) = self.decode_stream_with_encryption(&stream_obj, stream_ref) {
                    out.push((filename, bytes));
                }
            }
        }
        Ok(out)
    }

    /// Recursively collect `/Filespec` objects from an `/EmbeddedFiles`
    /// name-tree node: leaf `/Names [key filespec …]` pairs plus `/Kids`.
    fn collect_embedded_filespecs(&self, node: &Object, out: &mut Vec<Object>, depth: u8) {
        if depth > 32 {
            return;
        }
        let Ok(node) = self.resolve_object(node) else {
            return;
        };
        let Some(dict) = node.as_dict() else {
            return;
        };
        if let Some(names) = dict.get("Names").and_then(|n| self.resolve_object(n).ok()) {
            if let Some(arr) = names.as_array() {
                // Flat [key1 filespec1 key2 filespec2 …]: the odd indices.
                let mut i = 1;
                while i < arr.len() {
                    if let Ok(fs) = self.resolve_object(&arr[i]) {
                        out.push(fs);
                    }
                    i += 2;
                }
            }
        }
        if let Some(kids) = dict.get("Kids").and_then(|k| self.resolve_object(k).ok()) {
            if let Some(arr) = kids.as_array() {
                for kid in arr {
                    self.collect_embedded_filespecs(kid, out, depth + 1);
                }
            }
        }
    }

    /// Build the resource-name → colour-space-object map from a resolved
    /// `/Resources` dictionary's `/ColorSpace` subdictionary (§8.6.3 / §7.8.3),
    /// resolving one indirect-ref hop per entry so the stored value is a colour
    /// space name or array. Empty when there is no `/ColorSpace` subdictionary;
    /// the standard device names parse directly and need no entry. Consumed by
    /// the image-handle builders so `decode()` / the handle's `color_space` can
    /// resolve names like `/CS0` (§8.6.6, §8.9.7).
    fn build_color_space_map(
        &self,
        resources: Option<&Object>,
    ) -> std::collections::HashMap<String, Object> {
        let mut map = std::collections::HashMap::new();
        let Some(res) = resources else {
            return map;
        };
        let res = if let Some(r) = res.as_reference() {
            match self.load_object(r) {
                Ok(o) => o,
                Err(_) => return map,
            }
        } else {
            res.clone()
        };
        let Some(res_dict) = res.as_dict() else {
            return map;
        };
        let Some(cs_entry) = res_dict.get("ColorSpace") else {
            return map;
        };
        let cs_obj = if let Some(r) = cs_entry.as_reference() {
            match self.load_object(r) {
                Ok(o) => o,
                Err(_) => return map,
            }
        } else {
            cs_entry.clone()
        };
        let Some(cs_dict) = cs_obj.as_dict() else {
            return map;
        };
        for (name, value) in cs_dict.iter() {
            let resolved = if let Some(r) = value.as_reference() {
                self.load_object(r).unwrap_or_else(|_| value.clone())
            } else {
                value.clone()
            };
            map.insert(name.clone(), resolved);
        }
        map
    }

    /// Enumerate images on a page without decompressing any stream (Phase 1).
    ///
    /// Walks the page content stream once and reads image metadata (dimensions,
    /// colour space, filter chain, compressed size) directly from each Image
    /// XObject dictionary. No pixel data is decoded. Returns a handle per image
    /// in content-stream paint order.
    ///
    /// Call [`crate::PdfImageHandle::decode`] on individual handles to materialise only
    /// the images you need, or [`crate::PdfImageHandle::raw_compressed_bytes`] to forward
    /// compressed data (e.g. JPEG bytes) without recompression.
    ///
    /// Form XObjects (subtype `/Form`) are recursed into, matching the behaviour
    /// of [`PdfDocument::extract_images`]. Cycle detection (depth limit 100) and
    /// the document's Form stream cache are used. Images inside nested or shared
    /// Forms receive the correct final CTM-composed `bbox` / `rotation_degrees`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::PdfDocument;
    /// # let bytes = std::fs::read("page.pdf").unwrap();
    /// let doc = PdfDocument::from_bytes(bytes).unwrap();
    ///
    /// // Decode only images larger than a thumbnail threshold
    /// let images: Vec<_> = doc.page_image_handles(0)?
    ///     .into_iter()
    ///     .filter(|h| h.width >= 200 && h.height >= 200)
    ///     .map(|h| h.decode())
    ///     .collect::<Result<_, _>>()?;
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn page_image_handles(
        &self,
        page_index: usize,
    ) -> Result<Vec<crate::extractors::images::PdfImageHandle<'_>>> {
        use crate::content::parse_content_stream_images_only;
        use crate::content::Operator;
        use crate::extractors::images::image_handle_from_inline;

        self.require_authenticated()?;

        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        let content_data = self.get_page_content_data(page_index)?;

        let resources = match page_dict.get("Resources") {
            Some(res) => {
                if let Some(ref_obj) = res.as_reference() {
                    Some(self.load_object(ref_obj)?)
                } else {
                    Some(res.clone())
                }
            }
            None => None,
        };

        let operators = match parse_content_stream_images_only(&content_data) {
            Ok(ops) => ops,
            Err(_) => return Ok(Vec::new()),
        };

        // Resource-name colour-space map for this page scope (§8.6.6 / §8.9.7).
        let cs_map = self.build_color_space_map(resources.as_ref());

        // Pre-resolve the XObject dictionary once
        let xobject_dict = if let Some(ref res) = resources {
            if let Some(res_dict) = res.as_dict() {
                if let Some(xobj_entry) = res_dict.get("XObject") {
                    let resolved = if let Some(ref_obj) = xobj_entry.as_reference() {
                        self.load_object(ref_obj)?
                    } else {
                        xobj_entry.clone()
                    };
                    resolved.as_dict().cloned()
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let mut handles = Vec::new();
        let mut ctm_stack = vec![crate::content::Matrix::identity()];
        let mut paint_order: usize = 0;
        let mut xobject_stack: Vec<crate::object::ObjectRef> = Vec::new();

        for op in operators {
            match op {
                Operator::SaveState => {
                    if let Some(current) = ctm_stack.last() {
                        ctm_stack.push(*current);
                    }
                }
                Operator::RestoreState => {
                    if ctm_stack.len() > 1 {
                        ctm_stack.pop();
                    }
                }
                Operator::Cm { a, b, c, d, e, f } => {
                    if let Some(current) = ctm_stack.last_mut() {
                        let m = crate::content::Matrix { a, b, c, d, e, f };
                        *current = m.multiply(current);
                    }
                }
                Operator::Do { name } => {
                    if let Some(ref xobj_dict_map) = xobject_dict {
                        let ctm = ctm_stack
                            .last()
                            .copied()
                            .unwrap_or_else(crate::content::Matrix::identity);
                        if let Ok(mut more) = self.collect_handles_from_do(
                            &name,
                            xobj_dict_map,
                            resources.as_ref(),
                            ctm,
                            &mut paint_order,
                            &mut xobject_stack,
                        ) {
                            handles.append(&mut more);
                        }
                    }
                }
                Operator::InlineImage { dict, data } => {
                    let ctm = ctm_stack
                        .last()
                        .copied()
                        .unwrap_or_else(crate::content::Matrix::identity);
                    if let Some(handle) =
                        image_handle_from_inline(self, &dict, data, ctm, paint_order, &cs_map)
                    {
                        handles.push(handle);
                        paint_order += 1;
                    }
                }
                _ => {}
            }
        }

        Ok(handles)
    }

    /// Collect zero or more image handles for a `Do` operator.
    ///
    /// If the target is an Image XObject, returns a vec containing one handle
    /// (paint_order is advanced). If it is a Form XObject, recurses and returns
    /// all image handles found inside (including nested Forms), with correct
    /// paint_order and CTM composition for every handle.
    fn collect_handles_from_do<'s>(
        &'s self,
        name: &str,
        xobject_dict: &std::collections::HashMap<String, Object>,
        resources: Option<&Object>,
        ctm: crate::content::Matrix,
        paint_order: &mut usize,
        xobject_stack: &mut Vec<crate::object::ObjectRef>,
    ) -> Result<Vec<crate::extractors::images::PdfImageHandle<'s>>> {
        use crate::extractors::images::image_handle_from_xobject;

        let xobject_ref_obj = match xobject_dict.get(name) {
            Some(o) => o,
            None => return Ok(Vec::new()),
        };

        let xobject_ref_opt = xobject_ref_obj.as_reference();
        let xobject = if let Some(ref_obj) = xobject_ref_opt {
            self.load_object(ref_obj)?
        } else {
            xobject_ref_obj.clone()
        };
        let xobj_dict = match xobject.as_dict() {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };

        let subtype = xobj_dict
            .get("Subtype")
            .and_then(|s| s.as_name())
            .unwrap_or("");

        match subtype {
            "Image" => {
                if let Some(ref_obj) = xobject_ref_opt {
                    let cs_map = self.build_color_space_map(resources);
                    if let Some(h) = image_handle_from_xobject(
                        self,
                        ref_obj,
                        xobj_dict,
                        ctm,
                        *paint_order,
                        &cs_map,
                    ) {
                        *paint_order += 1;
                        Ok(vec![h])
                    } else {
                        Ok(Vec::new())
                    }
                } else {
                    Ok(Vec::new())
                }
            }
            "Form" => {
                if let (Some(ref_obj), Some(parent_res)) = (xobject_ref_opt, resources) {
                    self.collect_image_handles_from_form_xobject(
                        ref_obj,
                        &xobject,
                        parent_res,
                        ctm,
                        paint_order,
                        xobject_stack,
                    )
                } else {
                    Ok(Vec::new())
                }
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Recursively collect image handles from a Form XObject.
    ///
    /// This is the handles-side equivalent of `extract_images_from_form_xobject`.
    /// It uses the same cycle detection (ObjectRef stack + depth 100), the same
    /// Form Resources fallback rules, the same Form /Matrix handling, and reuses
    /// the document's xobject_stream_cache (50 MiB bound) for decompressed Form
    /// content.
    ///
    /// Unlike the materialised path, we do not cache "raw" handles — we compose
    /// the full CTM (`parent_ctm * form_matrix`) at entry and let every inner
    /// handle (and nested Form) naturally receive the final geometry. This is
    /// simpler for the two-phase API and produces correct `bbox`/`rotation_degrees`
    /// / `ctm` fields on the returned handles.
    fn collect_image_handles_from_form_xobject<'s>(
        &'s self,
        xobject_ref: crate::object::ObjectRef,
        xobject: &Object,
        parent_resources: &Object,
        parent_ctm: crate::content::Matrix,
        paint_order: &mut usize,
        xobject_stack: &mut Vec<crate::object::ObjectRef>,
    ) -> Result<Vec<crate::extractors::images::PdfImageHandle<'s>>> {
        use crate::content::parse_content_stream_images_only;
        use crate::content::Operator;
        use crate::extractors::images::image_handle_from_inline;

        // Cycle detection — identical policy to the materialised extraction path.
        if xobject_stack.contains(&xobject_ref) || xobject_stack.len() >= 100 {
            return Ok(Vec::new());
        }

        xobject_stack.push(xobject_ref);

        let xobj_dict = match xobject.as_dict() {
            Some(d) => d,
            None => {
                xobject_stack.pop();
                return Ok(Vec::new());
            }
        };

        // Form's own Resources (fallback to the parent's resources if absent).
        let form_resources = if let Some(form_res) = xobj_dict.get("Resources") {
            if let Some(ref_obj) = form_res.as_reference() {
                self.load_object(ref_obj)?
            } else {
                form_res.clone()
            }
        } else {
            parent_resources.clone()
        };

        // Pre-resolve the XObject dictionary for *this* Form's Resources.
        let form_xobject_dict = if let Some(res_dict) = form_resources.as_dict() {
            if let Some(xobj_entry) = res_dict.get("XObject") {
                let resolved = if let Some(ref_obj) = xobj_entry.as_reference() {
                    self.load_object(ref_obj)?
                } else {
                    xobj_entry.clone()
                };
                resolved.as_dict().cloned()
            } else {
                None
            }
        } else {
            None
        };

        // Form's own transformation matrix (default identity).
        let form_matrix = if let Some(matrix_obj) = xobj_dict.get("Matrix") {
            self.parse_matrix_from_object(matrix_obj)
                .unwrap_or_else(crate::content::Matrix::identity)
        } else {
            crate::content::Matrix::identity()
        };

        // Decode the Form stream (respecting the 50 MiB document-level cache).
        let cached_stream = self
            .xobject_stream_cache
            .lock_or_recover()
            .get(&xobject_ref)
            .cloned();
        let stream_data = if let Some(cached) = cached_stream {
            cached.as_ref().clone()
        } else {
            match self.decode_stream_with_encryption(xobject, xobject_ref) {
                Ok(data) => {
                    const MAX_STREAM_CACHE_BYTES: usize = 50 * 1024 * 1024;
                    let current_bytes = self.xobject_stream_cache_bytes.load(Ordering::Relaxed);
                    if current_bytes + data.len() <= MAX_STREAM_CACHE_BYTES {
                        self.xobject_stream_cache_bytes
                            .store(current_bytes + data.len(), Ordering::Relaxed);
                        self.xobject_stream_cache
                            .lock_or_recover()
                            .insert(xobject_ref, std::sync::Arc::new(data.clone()));
                    }
                    data
                }
                Err(e) => {
                    log::warn!("Failed to decode Form XObject stream: {}, skipping", e);
                    xobject_stack.pop();
                    return Ok(Vec::new());
                }
            }
        };

        // Parse with the fast images-only parser (same as the materialised path).
        let operators = match parse_content_stream_images_only(&stream_data) {
            Ok(ops) => ops,
            Err(_) => {
                xobject_stack.pop();
                return Ok(Vec::new());
            }
        };

        // Critical CTM composition:
        // Start the form's internal graphics state with `parent_ctm * form_matrix`.
        // Every image (and nested Form) discovered inside will then have its
        // handle's bbox/rotation/ctm computed with the *final* transform that
        // will be active when the image is painted on the page.
        let start_ctm = parent_ctm.multiply(&form_matrix);
        let mut ctm_stack = vec![start_ctm];
        let mut handles = Vec::new();

        for op in operators {
            match op {
                Operator::SaveState => {
                    if let Some(current) = ctm_stack.last() {
                        ctm_stack.push(*current);
                    }
                }
                Operator::RestoreState => {
                    if ctm_stack.len() > 1 {
                        ctm_stack.pop();
                    }
                }
                Operator::Cm { a, b, c, d, e, f } => {
                    if let Some(current) = ctm_stack.last_mut() {
                        let m = crate::content::Matrix { a, b, c, d, e, f };
                        *current = m.multiply(current);
                    }
                }

                Operator::Do { name } => {
                    if let Some(ref xobj_d) = form_xobject_dict {
                        let current_ctm = ctm_stack
                            .last()
                            .copied()
                            .unwrap_or_else(crate::content::Matrix::identity);
                        if let Ok(mut more) = self.collect_handles_from_do(
                            &name,
                            xobj_d,
                            Some(&form_resources),
                            current_ctm,
                            paint_order,
                            xobject_stack,
                        ) {
                            handles.append(&mut more);
                        }
                    }
                }

                Operator::InlineImage { dict, data } => {
                    let current_ctm = ctm_stack
                        .last()
                        .copied()
                        .unwrap_or_else(crate::content::Matrix::identity);
                    let cs_map = self.build_color_space_map(Some(&form_resources));
                    if let Some(h) = image_handle_from_inline(
                        self,
                        &dict,
                        data,
                        current_ctm,
                        *paint_order,
                        &cs_map,
                    ) {
                        handles.push(h);
                        *paint_order += 1;
                    }
                }

                _ => {}
            }
        }

        xobject_stack.pop();
        Ok(handles)
    }

    /// Extract images with pre-decompression filtering.
    ///
    /// Applies dimension and pixel-count checks using XObject dictionary metadata
    /// BEFORE expensive stream decompression. This avoids decompressing oversized
    /// images (e.g., 36MP presentation slides) or tiny glyph fragments that will
    /// be discarded downstream.
    fn extract_images_filtered(
        &self,
        page_index: usize,
        filter: &ImageExtractFilter,
    ) -> Result<Vec<crate::extractors::PdfImage>> {
        use crate::content::parse_content_stream_images_only;
        use crate::content::Operator;

        // Get page object and resources
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        // Get content stream
        let content_data = self.get_page_content_data(page_index)?;

        // Resolve resources
        let resources = match page_dict.get("Resources") {
            Some(res) => {
                if let Some(ref_obj) = res.as_reference() {
                    Some(self.load_object(ref_obj)?)
                } else {
                    Some(res.clone())
                }
            }
            None => None,
        };

        // Parse content stream with image-only fast path (skips BT/ET text blocks)
        let operators = match parse_content_stream_images_only(&content_data) {
            Ok(ops) => ops,
            Err(_) => {
                // If content stream parsing fails, return empty
                return Ok(Vec::new());
            }
        };

        let mut images = Vec::new();
        let mut ctm_stack = vec![crate::content::Matrix::identity()];
        // Shared cycle detection stack for Form XObject recursion.
        // This must persist across all Do operator calls to detect circular references
        // (e.g., Form X0 references X1 which references X0).
        let mut xobject_stack = Vec::new();

        // Pre-resolve XObject dictionary once (avoids re-resolving per Do operator)
        let xobject_dict = if let Some(ref res) = resources {
            if let Some(res_dict) = res.as_dict() {
                if let Some(xobj_entry) = res_dict.get("XObject") {
                    let resolved = if let Some(ref_obj) = xobj_entry.as_reference() {
                        self.load_object(ref_obj)?
                    } else {
                        xobj_entry.clone()
                    };
                    resolved.as_dict().cloned()
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // Parse content stream operators to extract images from Do operators
        for op in operators {
            match op {
                // Graphics state operators
                Operator::SaveState => {
                    if let Some(current_ctm) = ctm_stack.last() {
                        ctm_stack.push(*current_ctm);
                    }
                }
                Operator::RestoreState => {
                    if ctm_stack.len() > 1 {
                        ctm_stack.pop();
                    }
                }
                Operator::Cm { a, b, c, d, e, f } => {
                    if let Some(current_ctm) = ctm_stack.last_mut() {
                        let matrix = crate::content::Matrix { a, b, c, d, e, f };
                        // PDF spec ISO 32000-1:2008 §8.3.4: cm concatenates as M_cm × CTM
                        *current_ctm = matrix.multiply(current_ctm);
                    }
                }

                // XObject reference operator - Extract images referenced via Do
                Operator::Do { name } => {
                    if let Some(ref xobj_dict) = xobject_dict {
                        let current_ctm = ctm_stack
                            .last()
                            .copied()
                            .unwrap_or_else(crate::content::Matrix::identity);
                        if let Ok(mut xobj_images) = self.extract_images_from_xobject_do(
                            &name,
                            xobj_dict,
                            resources.as_ref(),
                            current_ctm,
                            &mut xobject_stack,
                            filter,
                        ) {
                            images.append(&mut xobj_images);
                        }
                    }
                }

                // Inline image operator
                Operator::InlineImage { dict, data } => {
                    let current_ctm = ctm_stack
                        .last()
                        .copied()
                        .unwrap_or_else(crate::content::Matrix::identity);
                    if let Ok(image) = self.extract_image_from_inline(&dict, &data, current_ctm) {
                        images.push(image);
                    }
                }

                _ => {} // Ignore other operators
            }
        }

        Ok(images)
    }

    /// Extract images referenced by a Do operator in the content stream.
    ///
    /// Accepts a pre-resolved XObject dictionary to avoid redundant lookups
    /// when called repeatedly (e.g., 194 Do operators on a single page).
    fn extract_images_from_xobject_do(
        &self,
        name: &str,
        xobject_dict: &std::collections::HashMap<String, Object>,
        resources: Option<&Object>,
        ctm: crate::content::Matrix,
        xobject_stack: &mut Vec<ObjectRef>,
        filter: &ImageExtractFilter,
    ) -> Result<Vec<crate::extractors::PdfImage>> {
        use crate::extractors::extract_image_from_xobject;

        let mut images = Vec::new();

        // Get the specific XObject by name
        let xobject_ref_obj = match xobject_dict.get(name) {
            Some(obj) => obj,
            None => return Ok(images), // Named XObject not found
        };

        // Load XObject (can be indirect reference or direct object)
        let xobject_ref_opt = xobject_ref_obj.as_reference();
        let xobject = if let Some(ref_obj) = xobject_ref_opt {
            self.load_object(ref_obj)?
        } else {
            xobject_ref_obj.clone()
        };
        let xobject_dict = xobject.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "XObject is not a dictionary".to_string(),
        })?;

        // Check Subtype
        let subtype = xobject_dict
            .get("Subtype")
            .and_then(|s| s.as_name())
            .unwrap_or("");

        match subtype {
            "Image" => {
                // Pre-decompression filtering using dictionary metadata.
                // These checks use Width/Height/ColorSpace from the XObject dictionary
                // which are available WITHOUT decompressing the image stream data.
                let w = xobject_dict
                    .get("Width")
                    .and_then(|o| o.as_integer())
                    .unwrap_or(0);
                let h = xobject_dict
                    .get("Height")
                    .and_then(|o| o.as_integer())
                    .unwrap_or(0);
                if w < filter.min_width || h < filter.min_height {
                    return Ok(images);
                }
                if (w as u64) * (h as u64) > filter.max_pixels {
                    return Ok(images);
                }
                // Skip small Indexed colorspace images (Type3 font glyph fragments)
                if filter.skip_indexed_small > 0
                    && (w < filter.skip_indexed_small || h < filter.skip_indexed_small)
                {
                    if let Some(cs_obj) = xobject_dict.get("ColorSpace") {
                        let is_indexed = match cs_obj {
                            Object::Name(n) => n == "Indexed",
                            Object::Array(arr) if !arr.is_empty() => {
                                arr[0].as_name() == Some("Indexed")
                            }
                            _ => false,
                        };
                        if is_indexed {
                            return Ok(images);
                        }
                    }
                }

                // Only clone+modify when ColorSpace needs resolving from indirect ref
                let needs_cs_resolve = matches!(
                    &xobject,
                    Object::Stream { dict, .. } if matches!(dict.get("ColorSpace"), Some(Object::Reference(_)))
                );

                let resolved_xobject;
                let xobject_for_extract = if needs_cs_resolve {
                    if let Object::Stream { dict, data } = &xobject {
                        let mut new_dict = dict.clone();
                        if let Some(Object::Reference(cs_ref)) = dict.get("ColorSpace") {
                            if let Ok(resolved_cs) = self.load_object(*cs_ref) {
                                new_dict.insert("ColorSpace".to_string(), resolved_cs);
                            }
                        }
                        resolved_xobject = Object::Stream {
                            dict: new_dict,
                            data: data.clone(),
                        };
                        &resolved_xobject
                    } else {
                        &xobject
                    }
                } else {
                    &xobject
                };

                // Extract as Image XObject
                if let Ok(mut image) = extract_image_from_xobject(
                    Some(self),
                    xobject_for_extract,
                    xobject_ref_opt,
                    None,
                ) {
                    // In PDF, images are mapped from unit square (0,0 to 1,1) to the CTM.
                    let unit_rect = crate::geometry::Rect::new(0.0, 0.0, 1.0, 1.0);
                    let bbox = self.transform_bbox_with_ctm(&unit_rect, ctm);
                    image.set_bbox(bbox);

                    // Capture transformation matrix and rotation (v0.3.14)
                    image.set_matrix([ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f]);
                    image.set_rotation_degrees(Self::matrix_to_rotation(ctm));

                    images.push(image);
                }
            }
            "Form" => {
                // Recursively extract from Form XObject
                // Only process if we have a valid reference and parent resources
                if let (Some(ref_obj), Some(parent_res)) = (xobject_ref_opt, resources) {
                    if let Ok(mut form_images) = self.extract_images_from_form_xobject(
                        ref_obj,
                        &xobject,
                        parent_res,
                        ctm,
                        xobject_stack,
                        filter,
                    ) {
                        images.append(&mut form_images);
                    }
                }
            }
            _ => {} // Skip other types (PS, etc.)
        }

        Ok(images)
    }

    /// Recursively extract images from a Form XObject.
    ///
    /// Uses a document-level cache: images are extracted once using only the Form's
    /// own Matrix, then cached. On subsequent references, cached images are cloned
    /// and the caller's CTM is applied to transform bboxes.
    fn extract_images_from_form_xobject(
        &self,
        xobject_ref: ObjectRef,
        xobject: &Object,
        parent_resources: &Object,
        parent_ctm: crate::content::Matrix,
        xobject_stack: &mut Vec<ObjectRef>,
        filter: &ImageExtractFilter,
    ) -> Result<Vec<crate::extractors::PdfImage>> {
        use crate::content::parse_content_stream_images_only;
        use crate::content::Operator;

        // Cycle detection
        if xobject_stack.contains(&xobject_ref) || xobject_stack.len() >= 100 {
            return Ok(Vec::new());
        }

        // Check image result cache — images stored with Form's own Matrix only.
        // Scope the borrow to ensure it's dropped before potential recursion.
        {
            if let Some(cached_images) = self
                .form_xobject_images_cache
                .lock_or_recover()
                .get(&xobject_ref)
            {
                let images = cached_images
                    .iter()
                    .map(|img| {
                        let mut cloned = img.clone();
                        if let Some(rect) = cloned.bbox() {
                            cloned.set_bbox(self.transform_bbox_with_ctm(rect, parent_ctm));
                        }
                        cloned
                    })
                    .collect();
                return Ok(images);
            }
        }

        xobject_stack.push(xobject_ref);

        let xobj_dict = xobject.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Form XObject is not a dictionary".to_string(),
        })?;

        // Get Form resources (with fallback to parent)
        let form_resources = if let Some(form_res) = xobj_dict.get("Resources") {
            if let Some(ref_obj) = form_res.as_reference() {
                self.load_object(ref_obj)?
            } else {
                form_res.clone()
            }
        } else {
            parent_resources.clone()
        };

        // Pre-resolve XObject dictionary for this form's resources
        let form_xobject_dict = if let Some(res_dict) = form_resources.as_dict() {
            if let Some(xobj_entry) = res_dict.get("XObject") {
                let resolved = if let Some(ref_obj) = xobj_entry.as_reference() {
                    self.load_object(ref_obj)?
                } else {
                    xobj_entry.clone()
                };
                resolved.as_dict().cloned()
            } else {
                None
            }
        } else {
            None
        };

        // Get Form transformation matrix (default to identity)
        let form_matrix = if let Some(matrix_obj) = xobj_dict.get("Matrix") {
            self.parse_matrix_from_object(matrix_obj)
                .unwrap_or_else(crate::content::Matrix::identity)
        } else {
            crate::content::Matrix::identity()
        };

        // Decode form stream — check cache first to avoid repeated decompression
        let cached_stream = self
            .xobject_stream_cache
            .lock_or_recover()
            .get(&xobject_ref)
            .cloned();
        let stream_data = if let Some(cached) = cached_stream {
            cached.as_ref().clone()
        } else {
            match self.decode_stream_with_encryption(xobject, xobject_ref) {
                Ok(data) => {
                    const MAX_STREAM_CACHE_BYTES: usize = 50 * 1024 * 1024;
                    let current_bytes = self.xobject_stream_cache_bytes.load(Ordering::Relaxed);
                    if current_bytes + data.len() <= MAX_STREAM_CACHE_BYTES {
                        self.xobject_stream_cache_bytes
                            .store(current_bytes + data.len(), Ordering::Relaxed);
                        self.xobject_stream_cache
                            .lock_or_recover()
                            .insert(xobject_ref, std::sync::Arc::new(data.clone()));
                    }
                    data
                }
                Err(e) => {
                    log::warn!("Failed to decode Form XObject stream: {}, skipping", e);
                    xobject_stack.pop();
                    return Ok(Vec::new());
                }
            }
        };

        // Parse operators using fast image-only path (skips text operators)
        let operators = match parse_content_stream_images_only(&stream_data) {
            Ok(ops) => ops,
            Err(_) => {
                xobject_stack.pop();
                return Ok(Vec::new());
            }
        };

        // Extract using only the Form's own Matrix (no parent_ctm yet).
        // This allows caching the results and applying different parent CTMs later.
        let mut raw_images = Vec::new();
        let mut ctm_stack = vec![form_matrix];

        for op in operators {
            match op {
                Operator::SaveState => {
                    if let Some(current_ctm) = ctm_stack.last() {
                        ctm_stack.push(*current_ctm);
                    }
                }
                Operator::RestoreState => {
                    if ctm_stack.len() > 1 {
                        ctm_stack.pop();
                    }
                }
                Operator::Cm { a, b, c, d, e, f } => {
                    if let Some(current_ctm) = ctm_stack.last_mut() {
                        let matrix = crate::content::Matrix { a, b, c, d, e, f };
                        // PDF spec ISO 32000-1:2008 §8.3.4: cm concatenates as M_cm × CTM
                        *current_ctm = matrix.multiply(current_ctm);
                    }
                }

                Operator::Do { name } => {
                    if let Some(ref xobj_d) = form_xobject_dict {
                        let current_ctm = ctm_stack
                            .last()
                            .copied()
                            .unwrap_or_else(crate::content::Matrix::identity);
                        // For nested Do operators, pass identity as parent_ctm since
                        // we're building raw (un-transformed) images for caching
                        if let Ok(mut xobj_images) = self.extract_images_from_xobject_do(
                            &name,
                            xobj_d,
                            Some(&form_resources),
                            current_ctm,
                            xobject_stack,
                            filter,
                        ) {
                            raw_images.append(&mut xobj_images);
                        }
                    }
                }

                Operator::InlineImage { dict, data } => {
                    let current_ctm = ctm_stack
                        .last()
                        .copied()
                        .unwrap_or_else(crate::content::Matrix::identity);
                    if let Ok(image) = self.extract_image_from_inline(&dict, &data, current_ctm) {
                        raw_images.push(image);
                    }
                }

                _ => {}
            }
        }

        xobject_stack.pop();

        // Cache the raw images (with Form's own Matrix applied, but no parent CTM)
        self.form_xobject_images_cache
            .lock_or_recover()
            .insert(xobject_ref, raw_images.clone());

        // Apply parent_ctm to produce final images for this call
        let images = raw_images
            .into_iter()
            .map(|mut img| {
                if let Some(rect) = img.bbox() {
                    img.set_bbox(self.transform_bbox_with_ctm(rect, parent_ctm));
                }
                img
            })
            .collect();

        Ok(images)
    }

    /// Extract an inline image from the content stream.
    fn extract_image_from_inline(
        &self,
        dict: &std::collections::HashMap<String, Object>,
        data: &[u8],
        ctm: crate::content::Matrix,
    ) -> Result<crate::extractors::PdfImage> {
        use crate::extractors::expand_inline_image_dict;

        // Expand abbreviated dictionary
        let expanded_dict = expand_inline_image_dict(dict.clone());

        // Build a temporary stream object from the dictionary and data
        let stream_obj = Object::Stream {
            dict: expanded_dict,
            data: bytes::Bytes::copy_from_slice(data),
        };

        // Use existing extraction logic
        let mut image =
            crate::extractors::extract_image_from_xobject(Some(self), &stream_obj, None, None)?;

        // In PDF, images are mapped from unit square (0,0 to 1,1) to the CTM.
        let unit_rect = crate::geometry::Rect::new(0.0, 0.0, 1.0, 1.0);
        let bbox = self.transform_bbox_with_ctm(&unit_rect, ctm);
        image.set_bbox(bbox);

        // Capture transformation matrix and rotation (v0.3.14)
        image.set_matrix([ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f]);
        image.set_rotation_degrees(Self::matrix_to_rotation(ctm));

        Ok(image)
    }

    /// Helper to derive rotation angle from transformation matrix.
    fn matrix_to_rotation(m: crate::content::Matrix) -> i32 {
        // Compute angle from CTM components (atan2(b, a))
        let angle_rad = m.b.atan2(m.a);
        let angle_deg = (angle_rad.to_degrees().round() as i32) % 360;
        if angle_deg < 0 {
            angle_deg + 360
        } else {
            angle_deg
        }
    }

    /// Transform a bounding box using CTM.
    ///
    /// Transforms all four corners and computes the axis-aligned bounding box,
    /// which correctly handles rotation, shear, and negative scaling.
    fn transform_bbox_with_ctm(
        &self,
        rect: &crate::geometry::Rect,
        ctm: crate::content::Matrix,
    ) -> crate::geometry::Rect {
        let x0 = rect.x;
        let y0 = rect.y;
        let x1 = rect.x + rect.width;
        let y1 = rect.y + rect.height;

        // Transform all four corners
        let tx0 = ctm.a * x0 + ctm.c * y0 + ctm.e;
        let ty0 = ctm.b * x0 + ctm.d * y0 + ctm.f;

        let tx1 = ctm.a * x1 + ctm.c * y0 + ctm.e;
        let ty1 = ctm.b * x1 + ctm.d * y0 + ctm.f;

        let tx2 = ctm.a * x0 + ctm.c * y1 + ctm.e;
        let ty2 = ctm.b * x0 + ctm.d * y1 + ctm.f;

        let tx3 = ctm.a * x1 + ctm.c * y1 + ctm.e;
        let ty3 = ctm.b * x1 + ctm.d * y1 + ctm.f;

        let min_x = tx0.min(tx1).min(tx2).min(tx3);
        let max_x = tx0.max(tx1).max(tx2).max(tx3);
        let min_y = ty0.min(ty1).min(ty2).min(ty3);
        let max_y = ty0.max(ty1).max(ty2).max(ty3);

        crate::geometry::Rect {
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        }
    }

    /// Parse a Matrix object from PDF.
    fn parse_matrix_from_object(&self, obj: &Object) -> Option<crate::content::Matrix> {
        if let Some(array) = obj.as_array() {
            if array.len() >= 6 {
                let mut values = [0.0f32; 6];
                for (i, val) in array.iter().take(6).enumerate() {
                    let num = if let Some(f) = val.as_real() {
                        f as f32
                    } else {
                        let i_val = val.as_integer()?;
                        i_val as f32
                    };
                    values[i] = num;
                }

                return Some(crate::content::Matrix {
                    a: values[0],
                    b: values[1],
                    c: values[2],
                    d: values[3],
                    e: values[4],
                    f: values[5],
                });
            }
        }
        None
    }

    // ========================================================================
    // Debug/profiling helpers — thin pub wrappers over internal methods.
    // Used by examples/debug_katalog.rs to break extract_spans into phases.
    // ========================================================================

    /// Public wrapper for `get_page` (normally private).
    /// Exposed for profiling examples that need to time page tree lookup separately.
    pub fn get_page_for_debug(&self, page_index: usize) -> Result<Object> {
        self.get_page(page_index)
    }

    /// Public wrapper for `may_contain_text` (normally pub(crate)).
    /// Returns true if the content stream might contain text operators (BT or Do).
    pub fn may_contain_text_public(data: &[u8]) -> bool {
        Self::may_contain_text(data)
    }

    /// Public wrapper for `load_fonts` (normally pub(crate)).
    /// Loads font dictionaries from a resources object into a TextExtractor.
    pub fn load_fonts_public(
        &self,
        resources: &Object,
        extractor: &mut crate::extractors::TextExtractor<'_>,
    ) -> Result<()> {
        self.load_fonts(resources, extractor)
    }

    /// Per-page mapping of PDF font-resource names (e.g. `"F75"`) to their
    /// canonical face name (e.g. `"TeXGyreTermesX-Regular"`, with any
    /// subset-prefix `ABCDEF+` stripped).
    ///
    /// Used by the layout-preserving DOCX writer so each text span can be
    /// emitted with the actual face name in `<w:rFonts>` instead of a
    /// PDF-internal resource id. The vector is `pages × map`; `map[i]`
    /// covers all fonts referenced by page `i`'s Resources.
    pub fn page_font_face_lookups(&self) -> Result<Vec<std::collections::HashMap<String, String>>> {
        use std::collections::HashMap;
        let n = self.page_count()?;
        let mut out: Vec<HashMap<String, String>> = Vec::with_capacity(n);
        for page_idx in 0..n {
            let mut lookup: HashMap<String, String> = HashMap::new();
            // Inline get_page → Resources so this works without `rendering`.
            let resources = match self.get_page(page_idx) {
                Ok(page) => match page.as_dict() {
                    Some(d) => {
                        let r = d
                            .get("Resources")
                            .cloned()
                            .unwrap_or(Object::Dictionary(std::collections::HashMap::new()));
                        if let Some(rref) = r.as_reference() {
                            self.load_object(rref)
                                .unwrap_or(Object::Dictionary(std::collections::HashMap::new()))
                        } else {
                            r
                        }
                    }
                    None => {
                        out.push(lookup);
                        continue;
                    }
                },
                Err(_) => {
                    out.push(lookup);
                    continue;
                }
            };
            let mut extractor = crate::extractors::TextExtractor::new();
            if self.load_fonts_public(&resources, &mut extractor).is_ok() {
                for (resource_name, info) in extractor.get_font_set() {
                    let canonical = info
                        .base_font
                        .split_once('+')
                        .map(|(_, rest)| rest)
                        .unwrap_or(info.base_font.as_str())
                        .to_string();
                    lookup.insert(resource_name, canonical);
                }
            }
            out.push(lookup);
        }
        Ok(out)
    }

    /// Extract every embedded font program (TrueType / OpenType bytes) used
    /// anywhere in the document, deduplicated by `BaseFont` name.
    ///
    /// Walks every page's font dictionary, loads each font via the same path
    /// `extract_text` uses, and returns the unique set of fonts that have
    /// embedded `FontFile2`/`FontFile3` streams. The `String` is the base
    /// font name (with any subset prefix like `ABCDEF+` stripped) and the
    /// `Vec<u8>` is the raw font program — directly suitable for re-embedding
    /// into another container (DOCX `word/fonts/`, another PDF, etc.).
    ///
    /// Fonts without embedded data (standard 14, missing FontFile streams)
    /// are skipped — there's nothing to extract.
    pub fn extract_embedded_fonts(&self) -> Result<Vec<(String, Vec<u8>)>> {
        use std::collections::HashMap;
        let mut by_name: HashMap<String, Vec<u8>> = HashMap::new();

        let n = self.page_count()?;
        for page_idx in 0..n {
            // Inline get_page_resources so this works without `rendering`.
            let resources = match self.get_page(page_idx) {
                Ok(page) => match page.as_dict() {
                    Some(d) => {
                        let r = d
                            .get("Resources")
                            .cloned()
                            .unwrap_or(Object::Dictionary(std::collections::HashMap::new()));
                        if let Some(rref) = r.as_reference() {
                            self.load_object(rref).unwrap_or_else(|_| {
                                Object::Dictionary(std::collections::HashMap::new())
                            })
                        } else {
                            r
                        }
                    }
                    None => continue,
                },
                Err(_) => continue,
            };
            let mut extractor = crate::extractors::TextExtractor::new();
            if self.load_fonts_public(&resources, &mut extractor).is_err() {
                continue;
            }
            for (_resource_name, font_arc) in extractor.get_font_set() {
                let Some(data) = font_arc.embedded_font_data.as_ref() else {
                    continue;
                };
                if data.is_empty() {
                    continue;
                }
                // Subset-prefix stripping: PDF font subsets carry a 6-letter
                // prefix followed by `+`, e.g. `ABCDEF+Calibri-Bold`. The
                // prefix is meaningless to consumers — strip it for dedup.
                let base = font_arc.base_font.as_str();
                let canonical = base.split_once('+').map(|(_, rest)| rest).unwrap_or(base);
                // When several subsets share a base name, `get_font_set()` yields
                // them in HashMap order, so `or_insert` kept a NONDETERMINISTIC
                // one - the returned bytes changed run to run for the same PDF.
                //
                // Choose by a TOTAL ORDER instead: largest program, ties broken
                // bytewise. Size is only a heuristic for "the richer subset" - a
                // program's byte count also grows with hinting and auxiliary
                // tables, so a larger subset is not necessarily a superset of a
                // smaller one. What the total order does guarantee is the property
                // callers actually depend on: the same PDF always yields the same
                // bytes.
                match by_name.entry(canonical.to_string()) {
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(data.as_ref().clone());
                    }
                    std::collections::hash_map::Entry::Occupied(mut o) => {
                        let cand = data.as_ref();
                        let cur = o.get();
                        if (cand.len(), cand.as_slice()) > (cur.len(), cur.as_slice()) {
                            *o.get_mut() = cand.clone();
                        }
                    }
                }
            }
        }

        let mut out: Vec<(String, Vec<u8>)> = by_name.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Like [`Self::extract_embedded_fonts`] but additionally returns a
    /// per-font Unicode → GID map reconstructed from the source PDF's
    /// `/ToUnicode` CMap and the font's CID/byte→GID table.
    ///
    /// CFF font subsets in PDFs (the typical Word/LibreOffice output)
    /// often ship without a Unicode cmap because CIDs encode the
    /// glyph stream directly. The font program parses fine but
    /// `EmbeddedFont::glyph_lookup` is empty; downstream font
    /// registration treats the font as unusable and falls back to
    /// Helvetica.
    ///
    /// The map returned here lets office_oxide / pdf_oxide write
    /// pipelines call [`crate::writer::EmbeddedFont::extend_glyph_lookup`]
    /// to re-populate the missing Unicode→GID entries from the
    /// source-PDF's own `/ToUnicode`. Result: CFF subset fonts
    /// register and render with the source typeface program instead
    /// of base-14 Helvetica.
    pub fn extract_embedded_fonts_with_unicode_maps(
        &self,
    ) -> Result<Vec<(String, Vec<u8>, std::collections::HashMap<u32, u16>)>> {
        let with_widths = self.extract_embedded_fonts_with_unicode_maps_and_widths()?;
        Ok(with_widths
            .into_iter()
            .map(|(name, data, uni, _widths)| (name, data, uni))
            .collect())
    }

    /// Like [`Self::extract_embedded_fonts_with_unicode_maps`] but also
    /// returns the per-glyph widths from the source PDF's `/W` array
    /// (in 1/1000 em units, keyed by GID). Required for re-embedding
    /// CFF font subsets whose synthetic OpenType wrapper carries no
    /// `hmtx` table — without this, ttf-parser returns 0 for every
    /// glyph advance and the round-trip writer emits a `/W` of zeros.
    pub fn extract_embedded_fonts_with_unicode_maps_and_widths(
        &self,
    ) -> Result<
        Vec<(
            String,
            Vec<u8>,
            std::collections::HashMap<u32, u16>,
            std::collections::HashMap<u16, u16>,
        )>,
    > {
        use std::collections::HashMap;
        let mut by_name: HashMap<String, (Vec<u8>, HashMap<u32, u16>, HashMap<u16, u16>)> =
            HashMap::new();

        let n = self.page_count()?;
        for page_idx in 0..n {
            let resources = match self.get_page(page_idx) {
                Ok(page) => match page.as_dict() {
                    Some(d) => {
                        let r = d
                            .get("Resources")
                            .cloned()
                            .unwrap_or(Object::Dictionary(std::collections::HashMap::new()));
                        if let Some(rref) = r.as_reference() {
                            self.load_object(rref).unwrap_or_else(|_| {
                                Object::Dictionary(std::collections::HashMap::new())
                            })
                        } else {
                            r
                        }
                    }
                    None => continue,
                },
                Err(_) => continue,
            };
            let mut extractor = crate::extractors::TextExtractor::new();
            if self.load_fonts_public(&resources, &mut extractor).is_err() {
                continue;
            }
            for (_resource_name, font_arc) in extractor.get_font_set() {
                let Some(data) = font_arc.embedded_font_data.as_ref() else {
                    continue;
                };
                if data.is_empty() {
                    continue;
                }
                let base = font_arc.base_font.as_str();
                let canonical = base.split_once('+').map(|(_, rest)| rest).unwrap_or(base);

                // Build Unicode → GID via ToUnicode CMap + GID resolver.
                //
                // We must consult the ToUnicode CMap *directly* rather than
                // going through `char_to_unicode`. `char_to_unicode` falls
                // through to a CID-as-Unicode fallback when the ToUnicode
                // CMap has no entry for a given code (Identity-H + Adobe-
                // Identity ordering, source font without a Unicode cmap).
                // That fallback returns spurious mappings like
                // U+0069 'i' → GID 105 (because CID 105 has no real
                // ToUnicode entry; the CID-as-Unicode path yields 'i'
                // for code=105 and the embedded TTF has no cmap to set us
                // straight). The spurious entries overwrite the real ones
                // we collected from CIDs that *do* have ToUnicode
                // entries (e.g. CID 0x4C → 'i', GID 76 for a
                // MicrosoftSansSerif subset) — which then makes the
                // injected cmap point Unicode codepoints at the wrong
                // glyph slots and the DOCX round-trip renders broken
                // lowercase letters.
                let mut uni_to_gid: HashMap<u32, u16> = HashMap::new();
                let to_unicode_cmap = font_arc.to_unicode.as_ref().and_then(|lazy| lazy.get());
                for code in 0u32..=0xFFFF {
                    // Require an authoritative ToUnicode entry. If the
                    // font has no ToUnicode CMap at all we conservatively
                    // skip injection — the fallback chain would only
                    // produce the misleading identity mapping.
                    let unicode_str =
                        match to_unicode_cmap.as_ref().and_then(|cmap| cmap.get(&code)) {
                            Some(s) if !s.is_empty() && s.as_ref() != "\u{FFFD}" => s.into_owned(),
                            _ => continue,
                        };
                    let cp = match unicode_str.chars().next() {
                        Some(c) => c as u32,
                        None => continue,
                    };
                    // Bare C0 controls (other than the legitimate
                    // whitespace handled in char_to_unicode) never name
                    // a real glyph — drop them so we don't inject a
                    // cmap entry that points U+0000..U+001F at random
                    // GIDs.
                    if matches!(cp, 0x00..=0x08 | 0x0B..=0x0C | 0x0E..=0x1F) {
                        continue;
                    }
                    // Only emit a Unicode→GID mapping when we have a
                    // real byte/CID → GID resolver from the source PDF.
                    // Falling back to identity for simple fonts whose
                    // CFF encoding parser couldn't extract a mapping
                    // produces a synthetic cmap that points Unicode at
                    // the wrong CFF charset positions: the round-trip
                    // emits Type0+Identity-H+CIDFontType0 and the
                    // viewer reads `glyph_at_charset[byte_code]`,
                    // which only equals the source glyph when CFF
                    // charset == StandardEncoding byte order — rarely
                    // true for subsetted CFF. Without a real mapping
                    // we leave the font un-patched, and office_oxide
                    // falls back to base-14 Helvetica via
                    // `EmbeddedFont::has_usable_unicode_cmap`.
                    let gid_opt = if let Some(ref map) = font_arc.cff_gid_map {
                        if code <= 0xFF {
                            map.get(&(code as u8)).copied()
                        } else {
                            None
                        }
                    } else if let Some(ref cid_map) = font_arc.cid_to_gid_map {
                        if code <= 0xFFFF {
                            Some(cid_map.get_gid(code as u16))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(gid) = gid_opt {
                        uni_to_gid.insert(cp, gid);
                    }
                }

                // Build GID → width from the source PDF's /W array.
                // For CIDFontType0+Identity-H: CID == GID directly.
                // For CIDFontType2: CID → GID via CIDToGIDMap.
                // For simple CFF (cff_gid_map): byte-code → GID.
                let mut gid_to_width: HashMap<u16, u16> = HashMap::new();
                if let Some(ref cid_widths) = font_arc.cid_widths {
                    if font_arc.cid_font_type.as_deref() == Some("CIDFontType0") {
                        for (&cid, &w) in cid_widths {
                            gid_to_width.insert(cid, w.round() as u16);
                        }
                    } else if let Some(ref cid_map) = font_arc.cid_to_gid_map {
                        for (&cid, &w) in cid_widths {
                            let gid = cid_map.get_gid(cid);
                            gid_to_width.insert(gid, w.round() as u16);
                        }
                    } else {
                        for (&cid, &w) in cid_widths {
                            gid_to_width.insert(cid, w.round() as u16);
                        }
                    }
                } else if let Some(ref cff_map) = font_arc.cff_gid_map {
                    // Simple CFF font: width-by-byte-code in font_arc.widths.
                    if let (Some(widths), Some(first)) =
                        (font_arc.widths.as_ref(), font_arc.first_char)
                    {
                        for (i, w) in widths.iter().enumerate() {
                            let byte = first + i as u32;
                            if byte > 0xFF {
                                break;
                            }
                            if let Some(&gid) = cff_map.get(&(byte as u8)) {
                                gid_to_width.insert(gid, w.round() as u16);
                            }
                        }
                    }
                }

                let entry = by_name
                    .entry(canonical.to_string())
                    .or_insert_with(|| (data.as_ref().clone(), HashMap::new(), HashMap::new()));
                // Same total-order choice as `extract_embedded_fonts`: largest
                // program, ties broken bytewise, rather than whichever HashMap
                // order surfaced first. (Size is a heuristic for the richer
                // subset, not a proof of superset - see the note there.)
                //
                // KNOWN GAP, deliberately left for a follow-up: the maps below
                // still merge across ALL subsets while the emitted program is now
                // a single chosen one, so a GID in the maps need not exist in the
                // program we hand back. Worse, when two subsets disagree about a
                // codepoint's GID - which subsets of one base font routinely do -
                // `or_insert` keeps whichever arrived first, so the maps carry the
                // very HashMap-order nondeterminism this fix removes from the
                // program. Fixing it means binding the maps to the chosen subset
                // instead of merging; that is a behaviour change (coverage may
                // shrink where subsets are disjoint) and belongs in its own PR.
                let cand = data.as_ref();
                if (cand.len(), cand.as_slice()) > (entry.0.len(), entry.0.as_slice()) {
                    entry.0 = cand.clone();
                }
                for (cp, gid) in uni_to_gid {
                    entry.1.entry(cp).or_insert(gid);
                }
                for (gid, w) in gid_to_width {
                    entry.2.entry(gid).or_insert(w);
                }
            }
        }

        let mut out: Vec<(String, Vec<u8>, HashMap<u32, u16>, HashMap<u16, u16>)> = by_name
            .into_iter()
            .map(|(name, (data, cmap, widths))| (name, data, cmap, widths))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

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
fn get_root_ref_from_trailer(trailer: &Object) -> Option<ObjectRef> {
    trailer.as_dict()?.get("Root")?.as_reference()
}

/// First in-use *uncompressed* object in the xref, used as a /Root-independent
/// probe for the garbage-prefix offset-shift decision. Compressed
/// entries can't be seek-validated, so they're skipped.
fn first_in_use_uncompressed(xref: &crate::xref::CrossRefTable) -> Option<ObjectRef> {
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
fn looks_like_prose_table(table: &crate::structure::Table) -> bool {
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
fn validate_object_at_offset<R: Read + Seek>(
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
fn validate_root_loadable<R: Read + Seek>(
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
fn has_standalone_obj_keyword(s: &str) -> bool {
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
fn parse_version_from_header(header: &[u8; 8], lenient: bool) -> Result<(u8, u8)> {
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

    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer)?;

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
fn find_substring(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }

    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
