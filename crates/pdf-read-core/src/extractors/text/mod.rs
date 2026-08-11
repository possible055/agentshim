//! Text extraction from PDF content streams.
//!
//! This module executes content stream operators to extract positioned
//! text characters with their Unicode mappings, font information,
//! bounding boxes.

#![forbid(unsafe_code)]

use crate::color::cmyk_to_rgb;
use crate::config::ExtractionProfile;
use crate::content::graphics_state::{GraphicsStateStack, Matrix};
use crate::content::operators::{Operator, TextElement};
use crate::content::parse_and_execute_text_only;
use crate::content::parse_content_stream;
use crate::content::parse_content_stream_text_only;
use crate::error::Result;
use crate::fonts::FontInfo;
use crate::geometry::Rect;
use crate::layout::{Color, FontWeight, TextChar, TextSpan};
use crate::object::{Object, ObjectRef};
use crate::pipeline::config::WordBoundaryMode;
use crate::text::{BoundaryContext, CharacterInfo, DocumentScript, WordBoundaryDetector};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod adaptive_and_artifacts;
mod color_operators;
mod decoding;
mod direction_and_gaps;
mod fonts_and_entry;
mod gap_geometry;
mod global;
mod glyphs;
mod lifecycle;
mod marked_content_operators;
mod merging_config;
mod operator_execution;
mod positioning;
mod spacing_config;
mod spacing_decision;
mod span_cleanup;
mod span_merging;
mod tj_processing;
mod word_splitting;
mod xobjects;

pub(crate) use gap_geometry::{
    is_monospace_font, is_pictographic, splits_one_word, starts_with_agl_ligature,
    strip_cjk_digit_boundary_spaces, strip_prime_decimal_boundary_spaces,
};
pub(crate) use global::preserve_unmapped_glyphs;
pub use global::set_preserve_unmapped_glyphs;
pub use merging_config::SpanMergingConfig;
pub use spacing_config::{SpaceDecision, SpaceSource, TextExtractionConfig};

use decoding::*;
use gap_geometry::*;
use spacing_decision::*;

/// Buffer for accumulating text from TJ array elements into a single span.
///
/// Per PDF Spec ISO 32000-1:2008, Section 9.4.4 NOTE 6:
/// "The performance of text searching (and other text extraction operations) is
/// significantly better if the text strings are as long as possible."
///
/// This buffer accumulates consecutive string elements from TJ arrays into
/// a single logical text span, only breaking on explicit word boundaries.
#[derive(Debug)]
struct TjBuffer {
    /// Accumulated Unicode text
    unicode: String,
    /// Text matrix at the start of this buffer
    start_matrix: Matrix,
    /// Font name when buffer started
    font_name: Option<String>,
    /// Fill color RGB when buffer started
    fill_color_rgb: (f32, f32, f32),
    /// Character spacing (Tc) when buffer started
    char_space: f32,
    /// Word spacing (Tw) when buffer started
    word_space: f32,
    /// Horizontal scaling (Th) when buffer started
    horizontal_scaling: f32,
    /// MCID when buffer started
    mcid: Option<u32>,
    /// Accumulated width from advance_position_for_string calls.
    /// Avoids redundant per-byte width recalculation in flush.
    accumulated_width: f32,
    /// Cached font reference — avoids per-Tj HashMap lookup in append.
    /// Set once at buffer creation, never changes (font change flushes buffer).
    cached_font: Option<Arc<FontInfo>>,
    /// Pre-computed effective font size (CTM × text_matrix scaling × font_size).
    /// Computed once at buffer creation to avoid matrix multiply + sqrt per flush.
    effective_font_size: f32,
    /// Pre-computed font weight from cached font reference.
    font_weight: FontWeight,
    /// Pre-computed italic flag from cached font reference.
    is_italic: bool,
    /// Whether the font is monospaced (from FixedPitch flag or name heuristic).
    is_monospace: bool,
    /// Per-character advance widths in text-space units (before user_h_scale).
    char_widths: Vec<f32>,
    /// Pre-computed user-space position (CTM applied to text matrix origin).
    /// Avoids two transform_point calls per flush.
    user_pos_x: f32,
    user_pos_y: f32,
    /// Pre-computed horizontal scale factor (CTM × text_matrix).
    /// Used to convert accumulated_width from text space to user space for bbox.
    user_h_scale: f32,
    /// Display rotation of this run in degrees, snapped to a quadrant when near
    /// one; `0.0` for ordinary horizontal text (see `snap_run_rotation`).
    rotation_degrees: f32,
    /// Writing mode (0 = horizontal, 1 = vertical) captured from the
    /// graphics state when the buffer started, so each emitted span
    /// carries the wmode it was rendered under. A font change flushes the
    /// buffer, so a single buffer never spans mixed writing modes.
    wmode: u8,
    /// Baseline shift as a ratio of font size (`Ts ÷ Tf size`, ISO 32000-1
    /// §9.3.7), captured from the graphics state when the buffer started.
    /// `> 0` superscript, `< 0` subscript, `0.0` on-baseline. Stored as a
    /// ratio so it is text/CTM-scale-independent and directly comparable to a
    /// font-size fraction by the sub/superscript rejoin.
    text_rise: f32,
    /// Text render mode (`Tr`, ISO 32000-1 §9.3.6), captured from the
    /// graphics state when the buffer started. `3`/`7` (invisible — neither
    /// filled nor stroked) means this run has no rendering-correctness
    /// pressure: an OCR-sandwich producer has no visual reason to mirror
    /// already-logical RTL glyph positions the way a *visible*-text
    /// producer would, so the geometric visual/logical detector's ascending-
    /// x signal is uninformative here (#826) — see `bidi::apply_rtl_verdict`.
    render_mode: u8,
}

/// Snap a run's display rotation (from the composed `CTM × T_m` rotation block,
/// `θ = atan2(b, a)`) to the nearest of `0 / 90 / 180 / -90` when it is within
/// `SNAP_TOL_DEG` of one, and treat everything within tolerance of `0` as exactly
/// horizontal (`0.0`). Mirrored text (negative matrix determinant) is reported as
/// its raw angle, not snapped, so it is never confused with a clean rotation.
fn snap_run_rotation(combined: &Matrix) -> f32 {
    const SNAP_TOL_DEG: f32 = 5.0;
    let (a, b, c, d) = (combined.a, combined.b, combined.c, combined.d);
    // Pure horizontal fast path (covers virtually all text): b and c ~ 0.
    if b.abs() < 1e-4 && c.abs() < 1e-4 {
        return 0.0;
    }
    let mut deg = b.atan2(a).to_degrees();
    // Normalise to (-180, 180].
    while deg > 180.0 {
        deg -= 360.0;
    }
    while deg <= -180.0 {
        deg += 360.0;
    }
    // Mirror (det < 0): leave the raw angle; the reading-order path treats any
    // non-zero rotation as a separate block regardless, and snapping a mirror to
    // a quadrant would misrepresent it.
    let det = a * d - b * c;
    if det < 0.0 {
        return if deg.abs() < SNAP_TOL_DEG { 0.0 } else { deg };
    }
    for &q in &[0.0_f32, 90.0, 180.0, -90.0] {
        if (deg - q).abs() <= SNAP_TOL_DEG {
            return q;
        }
    }
    deg
}

impl TjBuffer {
    /// Create a new empty buffer with current state.
    fn new(
        state: &crate::content::graphics_state::GraphicsState,
        mcid: Option<u32>,
        cached_font: Option<Arc<FontInfo>>,
    ) -> Self {
        // Pre-compute effective font size: CTM × text_matrix scaling × font_size
        let combined = state.ctm.multiply(&state.text_matrix);
        let effective_font_size =
            state.font_size * (combined.d * combined.d + combined.b * combined.b).sqrt();
        // Pre-compute horizontal scale for converting text-space widths to user space
        let user_h_scale = (combined.a * combined.a + combined.c * combined.c).sqrt();
        let font_weight = match &cached_font {
            Some(f) if f.is_bold() => FontWeight::Bold,
            _ => FontWeight::Normal,
        };
        let is_italic = cached_font.as_ref().map(|f| f.is_italic()).unwrap_or(false);
        let is_monospace = cached_font.as_ref().is_some_and(|f| {
            if f.flags.is_some_and(|flags| flags & 1 != 0) {
                return true;
            }
            let name = f.base_font.to_uppercase();
            name.contains("COURIER")
                || name.contains("CONSOLAS")
                || name.contains("MONO")
                || name.contains("FIXED")
        });
        let rotation_degrees = snap_run_rotation(&combined);
        // Pre-compute user-space position: text_matrix origin → CTM transform
        let text_pos = state.text_matrix.transform_point(0.0, 0.0);
        let user_pos = state.ctm.transform_point(text_pos.x, text_pos.y);
        Self {
            unicode: String::new(),
            start_matrix: state.text_matrix,
            font_name: state.font_name.clone(),
            fill_color_rgb: state.fill_color_rgb,
            char_space: state.char_space,
            word_space: state.word_space,
            horizontal_scaling: state.horizontal_scaling,
            mcid,
            accumulated_width: 0.0,
            cached_font,
            effective_font_size,
            font_weight,
            is_italic,
            is_monospace,
            char_widths: Vec::new(),
            user_pos_x: user_pos.x,
            user_pos_y: user_pos.y,
            user_h_scale,
            rotation_degrees,
            wmode: state.text_wmode,
            text_rise: if state.font_size > 0.0 {
                state.text_rise / state.font_size
            } else {
                0.0
            },
            render_mode: state.render_mode,
        }
    }

    /// Check if the buffer is empty.
    fn is_empty(&self) -> bool {
        self.unicode.is_empty()
    }

    /// Append a text string to the buffer.
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        // PDF spec Section 7.3.4.2: implementation limit of 32,767 bytes per string.
        // Malformed PDFs may exceed this, causing text blowup.
        let bytes = if bytes.len() > 32_767 {
            &bytes[..32_767]
        } else {
            bytes
        };

        let font = self.cached_font.as_deref();

        // Fast path: OneByte fonts push chars directly into buffer via lookup table.
        // Avoids String allocation in decode_text_to_unicode (2 allocations per call).
        if let Some(font) = font {
            if font.subtype != "Type0" {
                // #317 UTF-8-in-simple-font detection — see long comment in
                // `append_advance_buffer`. Some producers emit UTF-8 byte
                // sequences inside PDF string literals for fonts that only
                // declare a Latin encoding with no ToUnicode CMap. When the
                // entire byte slice is valid UTF-8 whose decoded chars
                // include at least one non-Latin-1 codepoint, treat it as
                // UTF-8 so we recover Cyrillic / Greek / CJK instead of
                // Latin-1 mojibake.
                if font.to_unicode.is_none() && bytes.len() >= 2 {
                    let has_high = bytes.iter().any(|&b| b >= 0x80);
                    if has_high {
                        if let Ok(decoded) = std::str::from_utf8(bytes) {
                            if decoded.chars().any(|c| c as u32 > 0xFF) {
                                for ch in decoded.chars() {
                                    self.unicode.push(ch);
                                }
                                return Ok(());
                            }
                        }
                    }
                }

                let table = font.get_byte_to_char_table();
                for &byte in bytes {
                    let c = table[byte as usize];
                    if c != '\0' {
                        self.unicode.push(c);
                    } else {
                        // Rare: multi-char mapping or unmapped byte
                        if let Some(s) = font.char_to_unicode(byte as u32) {
                            if s != "\u{FFFD}" || preserve_unmapped_glyphs() {
                                for ch in s.chars() {
                                    if ch >= '\x20' || ch == '\t' || ch == '\n' || ch == '\r' {
                                        self.unicode.push(ch);
                                    }
                                }
                            }
                        } else {
                            let fb = fallback_char_to_unicode(byte as u32);
                            if fb != "\u{FFFD}" || preserve_unmapped_glyphs() {
                                for ch in fb.chars() {
                                    if ch >= '\x20' || ch == '\t' || ch == '\n' || ch == '\r' {
                                        self.unicode.push(ch);
                                    }
                                }
                            }
                        }
                    }
                }
                return Ok(());
            }
        }

        // Slow path: Type0 (CID) fonts or no font — use full decode function
        let unicode_text = decode_text_to_unicode(bytes, font);
        self.unicode.push_str(&unicode_text);

        Ok(())
    }
}

/// Artifact type classification per PDF Spec Section 14.8.2.2
///
/// Artifacts are content that is not part of the document's logical structure,
/// such as headers, footers, page numbers, and decorative elements.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum ArtifactType {
    /// Pagination artifacts (headers, footers, page numbers)
    Pagination(PaginationSubtype),
    /// Layout artifacts (ruled lines, backgrounds, borders)
    Layout,
    /// Page artifacts (full-page backgrounds, watermarks)
    Page,
    /// Background graphics or decorations
    Background,
}

/// Pagination artifact subtypes per PDF Spec Section 14.8.2.2.1
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum PaginationSubtype {
    /// Page header content
    Header,
    /// Page footer content
    Footer,
    /// Watermark overlay
    Watermark,
    /// Page number
    PageNumber,
    /// Other pagination element
    Other,
}

/// Context for marked content sequences (per PDF Spec Section 14.6)
///
/// Tracks nested marked content tags to implement artifact filtering.
/// When content is marked as `/Artifact`, it should be excluded from text extraction.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
struct MarkedContentContext {
    tag: String,
    is_artifact: bool,
    /// Artifact type classification for filtered content (PDF Spec Section 14.8.2.2)
    artifact_type: Option<ArtifactType>,
    /// ActualText for marked content (PDF Spec Section 14.9.4)
    /// Used to replace extracted text with correct representation
    /// e.g., ligatures (fi, fl, ffi, ffl), decorated glyphs
    actual_text: Option<String>,
    /// True once an ActualText replacement has been emitted from this
    /// MC scope. Per ISO 32000-1:2008 §14.9.4 the `/ActualText` of a
    /// marked-content sequence is the replacement for the ENTIRE
    /// sequence — even if it contains multiple `Tj` / `TJ` operators
    /// the replacement is emitted ONCE. The first Tj inside a scope
    /// flips this flag; subsequent Tj operators see it and skip the
    /// replacement path.
    actual_text_emitted: bool,
    /// Expansion text for abbreviations (PDF Spec Section 14.9.5)
    /// The /E entry provides the expansion of an abbreviation or acronym.
    /// e.g., "PDF" might expand to "Portable Document Format"
    expansion: Option<String>,
    /// Whether this marked content context is an excluded Optional Content Group (layer).
    ///
    /// Set when tag is "OC" and the OCG /Name matches one of the excluded layers.
    is_excluded_layer: bool,
    /// Whether this marked content context is an InDesign "placed PDF" figure.
    ///
    /// Set when the tag is `/PlacedPDF` — an Adobe InDesign-specific
    /// marked-content tag that wraps an imported/placed PDF rendered AS a
    /// figure (always nested inside a `/Figure` structure element). Its text
    /// content is the placed artwork's own glyphs (e.g. a draft galley of the
    /// manuscript with line numbers), NOT the document's logical text — the
    /// authoritative copy is re-typeset outside the placed region. Treating it
    /// as a figure (suppressing its text) matches what pdftotext/PyMuPDF do
    /// and removes duplicated / mojibake overlay text. See `is_content_suppressed`.
    is_placed_pdf: bool,
    /// MCID declared by this BDC (only BDC; BMC carries no /MCID).
    ///
    /// Stored here so EMC can restore the outer scope's MCID instead
    /// of blanking `current_mcid` unconditionally. A `Tj` issued
    /// AFTER an inner EMC must still attribute to its enclosing
    /// MCID-bearing scope (the PDF spec specifies marked-content
    /// nesting at §14.6).
    own_mcid: Option<u32>,
}

/// Text extractor that processes content streams.
///
/// This structure maintains the graphics state stack and font information
/// while processing operators to extract positioned text.
///
/// The extractor can work in two modes:
/// - **Span mode** (default): Extracts complete text strings as PDF provides them (PDF spec compliant)
/// - **Character mode**: Extracts individual characters (for special use cases)
#[derive(Debug)]
pub struct TextExtractor<'doc> {
    /// Graphics state stack for handling q/Q operators
    state_stack: GraphicsStateStack,
    /// Loaded fonts (name -> FontInfo). Arc-wrapped to avoid deep cloning across pages.
    fonts: HashMap<String, Arc<FontInfo>>,
    /// Extracted text spans (complete strings from Tj/TJ operators)
    spans: Vec<TextSpan>,
    /// Extracted characters (for backward compatibility)
    chars: Vec<TextChar>,
    /// Operators executed since the last budget checkpoint.
    ///
    /// The content-stream loop is where both time and page memory accumulate, and it was
    /// the one loop with no checkpoint in it: a cancelled call could not stop until the
    /// whole stream had been executed, and a dense page could accumulate spans without
    /// limit. Counted rather than checked every operator because touching the budget
    /// would otherwise cost more than executing a cheap operator.
    operators_since_checkpoint: usize,
    /// Resources dictionary (for accessing XObjects and fonts)
    resources: Option<Object>,
    /// Reference to the document (for loading XObjects)
    document: Option<&'doc crate::document::PdfDocument>,
    /// Set of processed XObject references to avoid duplicates.
    /// Key is `(ObjectRef, ctm_key)` where `ctm_key` is the CTM at the time of
    /// the `Do` operator call, encoded as 6 millipoint-rounded i64 values.
    /// Using the CTM as part of the key allows the same Form XObject to be
    /// processed multiple times when invoked with different transformation
    /// matrices (e.g., the same XObject stamped at different positions on a page),
    /// while still preventing infinite recursion (same ref + same CTM).
    processed_xobjects: HashSet<(ObjectRef, [i64; 6])>,
    /// Cached XObject name → ObjectRef mapping for current resources context.
    /// Avoids expensive repeated resolution of the resources/XObject dict chain.
    cached_xobject_refs: HashMap<String, Option<ObjectRef>>,
    /// Current XObject recursion depth (0 = page level)
    xobject_depth: u32,
    /// Number of XObjects decoded on this page (for budget limiting)
    xobject_decode_count: u32,
    /// Configuration for text extraction heuristics
    config: TextExtractionConfig,
    /// Configuration for span merging behavior
    merging_config: SpanMergingConfig,
    /// Current marked content ID (for Tagged PDFs)
    ///
    /// Tracks the MCID of the currently active marked content sequence.
    /// Used to associate extracted text with structure tree elements.
    current_mcid: Option<u32>,
    /// Set of MCIDs whose BDC carried inline `/ActualText` on this
    /// page.
    ///
    /// Populated by the BDC handler whenever it observes
    /// `/ActualText` on the properties dictionary. The struct-tree-
    /// scope ActualText applier (in `document.rs`) uses this set to
    /// honour MC-scope-wins precedence: an ancestor StructElem's
    /// `/ActualText` must NOT override an MCID whose in-stream
    /// /ActualText has already been applied at extraction time
    /// (ISO 32000-1:2008 §14.6, §14.9.4).
    mc_actualtext_mcids: HashSet<u32>,
    /// Stack of marked content contexts (per PDF Spec Section 14.6)
    ///
    /// Tracks nested marked content tags to enable artifact filtering.
    /// When content is marked as `/Artifact`, it should be excluded from text extraction.
    marked_content_stack: Vec<MarkedContentContext>,
    /// True once a `/ReversedChars` marked-content sequence (ISO 32000-1
    /// §14.8.2.3.3) has been seen on this page. Such producers draw RTL glyphs
    /// individually with explicit positioning and mark real word boundaries with
    /// explicit space glyphs — so oxide must NOT additionally insert geometric
    /// word spaces between cursively-adjacent Arabic letters (which would shatter
    /// words, e.g. `إسبريسو` → `إس بر يسو`).
    saw_reversed_chars: bool,
    /// Whether we're currently inside an /Artifact marked content context
    ///
    /// Per PDF Spec Section 14.6, artifact content should be excluded from text extraction.
    /// This flag is true when any ancestor in the marked_content_stack has is_artifact=true.
    inside_artifact: bool,
    /// Layer names (Optional Content Groups) to exclude from extraction.
    ///
    /// When a BDC operator with tag "OC" references an OCG whose /Name matches
    /// one of these entries, all content within that marked content scope is suppressed.
    excluded_layers: HashSet<String>,
    /// Whether we're currently inside an excluded OCG layer.
    ///
    /// True when any ancestor in the marked_content_stack has is_excluded_layer=true.
    inside_excluded_layer: bool,
    /// Whether we're currently inside an InDesign `/PlacedPDF` figure region.
    ///
    /// True when any ancestor in the marked_content_stack has is_placed_pdf=true.
    /// Text inside a placed-PDF figure is the placed artwork's own glyphs and is
    /// suppressed (it is a figure, not logical text). See `MarkedContentContext::is_placed_pdf`.
    inside_placed_pdf: bool,
    /// When true, `/PlacedPDF` text is KEPT instead of suppressed for this page.
    ///
    /// The placed-PDF suppression assumes the placed region is a *decorative
    /// figure overlay* whose glyphs duplicate logical text that lives OUTSIDE it
    /// (the PMC8100493 draft-galley case). But some publishers (e.g. MATEC Web of
    /// Conferences) place the ENTIRE article body inside a single `/PlacedPDF`
    /// region, leaving almost nothing outside — there the placed text IS the
    /// page's logical content and suppressing it drops the whole page. Set by a
    /// cheap page-content-stream pre-scan (`placed_pdf_text_dominates`) that
    /// flips this on only when the placed text dominates and the non-placed text
    /// is negligible. pymupdf/pdftotext likewise extract the body in that case.
    placed_pdf_keep: bool,
    /// Ink / separation names to exclude from extraction.
    ///
    /// When a `cs` operator sets a Separation or DeviceN color space whose ink name(s)
    /// match one of these entries, subsequent text is suppressed until the color space changes.
    excluded_inks: HashSet<String>,
    /// Whether the current fill color space is an excluded ink.
    ///
    /// Set when SetFillColorSpace resolves to a Separation or DeviceN color space
    /// whose ink name(s) intersect with `excluded_inks`.
    inside_excluded_ink: bool,
    /// Extraction mode: true for spans, false for characters
    extract_spans: bool,
    /// Buffer for accumulating consecutive Tj operators into single spans
    ///
    /// Per PDF Spec ISO 32000-1:2008 Section 9.4.4 NOTE 6, text strings should
    /// be as long as possible. This buffer accumulates consecutive Tj operators
    /// until a positioning command or state change is encountered.
    tj_span_buffer: Option<TjBuffer>,
    /// Sequence counter for TextSpan ordering
    ///
    /// Used as a tie-breaker when sorting spans by Y-coordinate. Ensures
    /// that spans with identical Y-coordinates maintain extraction order.
    span_sequence_counter: usize,
    /// History of TJ array offsets for statistical analysis
    ///
    /// Tracks TJ offset values to detect justified vs. normal text through
    /// statistical distribution analysis (coefficient of variation).
    /// Used to dynamically adjust spacing thresholds per ISO 32000-1:2008 Section 9.4.4.
    tj_offset_history: Vec<f32>,
    /// Running sum / sum-of-squares so `analyze_tj_distribution` is O(1) rather
    /// than re-scanning the offset history (called once per TJ offset → O(n²)
    /// per page). `tj_stats_len` is the history length they cover; if the
    /// history is replaced wholesale, `analyze` recomputes once. f64 for precision.
    tj_sum: f64,
    tj_sum_sq: f64,
    tj_stats_len: usize,
    /// Character-level tracking for word boundary detection
    ///
    /// Collects CharacterInfo for each character during TJ array processing.
    /// This provides character-level positioning, width, and TJ offset data
    /// to WordBoundaryDetector for primary word boundary detection.
    /// Per ISO 32000-1:2008 Section 9.4.4, character-level analysis improves accuracy.
    tj_character_array: Vec<CharacterInfo>,
    /// Current X position in text space for character tracking
    ///
    /// Updated as each character in a TJ array is processed. Used to calculate
    /// x_position for CharacterInfo entries (not used after character collection).
    current_x_position: f32,
    /// Word boundary detection mode
    ///
    /// Controls whether WordBoundaryDetector is used as:
    /// - Tiebreaker: Only when TJ and geometric signals conflict (default)
    /// - Primary: Before creating TextSpans from tj_character_array
    word_boundary_mode: WordBoundaryMode,
    /// Cached current font (updated on Tf). Avoids per-Tj HashMap lookup
    /// in advance_position_for_string.
    cached_current_font: Option<Arc<FontInfo>>,
    /// Stack of MCID content-stream scopes (ISO 32000-1:2008 §14.7.4.3).
    ///
    /// Bottom of the stack is the page's own content-stream scope
    /// (`McidScope::Page(page_index)`). Each entry into a Form XObject
    /// via `Do` pushes a `McidScope::Form(form_ref)`; the matching
    /// pop restores the outer scope. The top of the stack stamps every
    /// `TextSpan` emitted while it is active. Tiling-Pattern walks are
    /// not currently traversed by the extractor (patterns rasterize
    /// independently); the spec-strict three-variant scope still
    /// covers `Pattern(_)` in the data model so future pattern-content
    /// walks can populate it.
    mcid_scope_stack: Vec<crate::structure::McidScope>,
}

impl<'doc> TextExtractor<'doc> {
    /// Fraction of a glyph's advance width considered "overlap" for
    /// duplicate detection. Used by both `deduplicate_overlapping_chars`
    /// and `deduplicate_overlapping_spans`.
    ///
    /// 0.30 comfortably catches real render-pass duplicates
    /// (stroke+fill, bold shadow, outline+fill) which sit well under
    /// 5 % of one advance apart, while staying below typical heaviest
    /// kerning (≤ 20 % of advance) so legitimate narrow-glyph
    /// neighbours (`ll`, `rr`, `II`, `ii`) are preserved.
    const DEDUP_OVERLAP_RATIO: f32 = 0.30;

    /// Absolute cap on the overlap window (in PDF points).
    ///
    /// Preserves pre-ratio v0.3.x behaviour for pathologically
    /// oversized advance values (drop-caps, large display text) where
    /// 30 % of the advance would swallow legitimate neighbours.
    const DEDUP_OVERLAP_CAP_PT: f32 = 2.0;
}

impl<'doc> Default for TextExtractor<'doc> {
    fn default() -> Self {
        Self::new()
    }
}
#[cfg(test)]
mod tests;

#[cfg(test)]
mod configuration_tests;

#[cfg(test)]
mod profile_based_space_tests;
