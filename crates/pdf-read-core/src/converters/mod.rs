//! Markdown conversion support.

pub use crate::pipeline::config::BoldMarkerBehavior;

/// Options used by the internal Markdown conversion pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversionOptions {
    /// Preserve additional horizontal spacing in Markdown.
    pub preserve_layout: bool,

    /// Automatically detect headings based on font size and weight.
    ///
    /// When true, uses font clustering to identify heading levels (H1, H2, H3).
    /// When false, treats all text as paragraphs.
    pub detect_headings: bool,

    /// Extract tables from the document.
    ///
    /// Note: Table extraction is currently not fully implemented.
    pub extract_tables: bool,

    /// Strip repeated running headers/footers from untagged documents (WS2.6).
    ///
    /// When true, a cross-page pass finds top/bottom-band text lines that
    /// recur on a majority of pages (page numbers ignored) and drops them from
    /// the output — the geometric counterpart to the `/Artifact`-tag filtering
    /// that already handles tagged PDFs. Off by default (behaviour change).
    pub strip_running_headers_footers: bool,

    /// Reading order determination mode.
    ///
    /// Controls how text blocks are ordered in the output.
    pub reading_order_mode: ReadingOrderMode,

    /// Control how bold markers are applied in markdown conversion.
    ///
    /// Determines whether bold formatting markers are applied to whitespace-only
    /// content (Aggressive) or only to content-bearing text (Conservative).
    /// See BoldMarkerBehavior for details.
    pub bold_marker_behavior: BoldMarkerBehavior,

    /// Configuration for spatial table detection.
    ///
    /// If None, uses default configuration.
    /// Only applies when extract_tables = true.
    pub table_detection_config: Option<crate::structure::TableDetectionConfig>,

    /// Include form field values inline in output.
    ///
    /// When true (default), form field values (text fields, checkboxes, choice fields)
    /// are converted to TextSpans at their spatial positions and merged with page content.
    /// This makes field values appear where they visually belong on the page.
    ///
    /// When false, form field values are omitted from output.
    pub include_form_fields: bool,

    /// Rectangular regions to exclude from text extraction.
    ///
    /// Spans whose bounding boxes match any region under `exclude_regions_mode`
    /// are dropped before the text-assembly pipeline runs. Use this to strip
    /// figure captions, sidebars, or any other spatially-identified region from
    /// the extracted text stream.
    ///
    /// For Tagged PDFs the extractor already honours `/Artifact` marked-content
    /// sequences (PDF spec ISO 32000-1 §14.8.2.2). This field provides the same
    /// capability for untagged PDFs where spatial coordinates are the only way
    /// to identify non-body regions.
    ///
    /// Note: exclusion is unconditional — a span inside a region is dropped
    /// regardless of its structure-tree role. For `MinOverlap(t)`, `t` is the
    /// fraction of the *span's* area that must overlap the excluded region.
    ///
    /// Default: empty (no regions excluded).
    pub exclude_regions: Vec<crate::geometry::Rect>,

    /// Overlap rule used when matching spans against `exclude_regions`.
    ///
    /// Default: [`crate::layout::RectFilterMode::Intersects`] — drops any span with any overlap.
    pub exclude_regions_mode: crate::layout::RectFilterMode,

    /// Restrict text extraction to a single rectangular region.
    ///
    /// When `Some((rect, mode))`, only spans that match `rect` under `mode`
    /// are kept before the text-assembly pipeline runs. This powers
    /// [`crate::document::PdfDocument::extract_text_in_rect`] so it produces fully-assembled
    /// output (line breaks, tables, reading order) rather than a flat word
    /// stream.
    ///
    /// Applied after `exclude_regions` so exclusions take precedence.
    ///
    /// Default: `None` (all spans kept).
    pub include_region: Option<(crate::geometry::Rect, crate::layout::RectFilterMode)>,

    /// Expand Unicode ligature characters to their component letters.
    ///
    /// When `true`, ligature characters from the Latin Alphabetic Presentation
    /// Forms block (U+FB00–U+FB06) are expanded to their ASCII equivalents:
    /// `ﬁ`→`fi`, `ﬂ`→`fl`, `ﬀ`→`ff`, `ﬃ`→`ffi`, `ﬄ`→`ffl`, `ﬅ`→`st`, `ﬆ`→`st`.
    ///
    /// When `false` (default), these characters are preserved exactly as the
    /// font's ToUnicode map produced them. Ground-truth corpora for PDF quality
    /// testing usually preserve ligatures, so the default avoids Jaccard penalty.
    ///
    /// Default: `false`.
    pub expand_ligatures: bool,

    /// Include spans tagged `/Artifact` (running headers/footers, page
    /// numbers, watermarks; ISO 32000-1:2008 §14.8.2.2.1) in the output.
    ///
    /// Default **`true`** for backward compatibility with pre-0.3.42
    /// behavior — the same rationale already shipped on
    /// [`crate::document::PdfDocument::extract_words_with_thresholds`] /
    /// `extract_text_lines`: flipping the default would surface as a
    /// content regression on PDFs whose running-artifact heuristic
    /// over-triggers on real content (e.g. a repeated footer that carries
    /// a section identifier, not just decoration). Set `false` to get the
    /// spec-correct behavior (artifact-tagged spans excluded).
    pub include_artifacts: bool,
}

impl Default for ConversionOptions {
    /// Create default conversion options.
    ///
    /// Defaults:
    /// - preserve_layout: false (semantic mode)
    /// - detect_headings: true (enabled for proper markdown output)
    /// - extract_tables: true
    /// - reading_order_mode: StructureTreeFirst (PDF-spec-compliant for Tagged PDFs, falls back to XY-Cut for untagged)
    /// - bold_marker_behavior: Conservative (no bold markers for whitespace-only content)
    /// - table_detection_config: None (uses defaults when table detection is enabled)
    /// - include_form_fields: true
    fn default() -> Self {
        Self {
            preserve_layout: false,
            detect_headings: true,
            extract_tables: true,
            strip_running_headers_footers: false,
            reading_order_mode: ReadingOrderMode::StructureTreeFirst { mcid_order: vec![] },
            bold_marker_behavior: BoldMarkerBehavior::Conservative,
            table_detection_config: None,
            include_form_fields: true,
            exclude_regions: Vec::new(),
            exclude_regions_mode: crate::layout::RectFilterMode::Intersects,
            include_region: None,
            expand_ligatures: false,
            include_artifacts: true,
        }
    }
}

impl ConversionOptions {
    /// Enable table detection with custom configuration.
    ///
    /// Sets extract_tables = true and uses the provided configuration.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::converters::ConversionOptions;
    /// use pdf_oxide::structure::TableDetectionConfig;
    ///
    /// let config = TableDetectionConfig::strict();
    /// let opts = ConversionOptions::default().with_table_detection(config);
    ///
    /// assert!(opts.extract_tables);
    /// assert!(opts.table_detection_config.is_some());
    /// ```
    pub fn with_table_detection(mut self, config: crate::structure::TableDetectionConfig) -> Self {
        self.extract_tables = true;
        self.table_detection_config = Some(config);
        self
    }

    /// Enable table detection with default configuration.
    ///
    /// Sets extract_tables = true and table_detection_config = None,
    /// which will use the default TableDetectionConfig when detection runs.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::converters::ConversionOptions;
    ///
    /// let opts = ConversionOptions::default().with_default_table_detection();
    ///
    /// assert!(opts.extract_tables);
    /// assert!(opts.table_detection_config.is_none());
    /// ```
    pub fn with_default_table_detection(mut self) -> Self {
        self.extract_tables = true;
        self.table_detection_config = None;
        self
    }
}

/// Reading order determination mode for text blocks.
///
/// Determines how text blocks are ordered when converting to output formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadingOrderMode {
    /// Simple top-to-bottom, left-to-right ordering.
    ///
    /// Sorts all blocks by Y coordinate (top to bottom), then by X coordinate (left to right).
    /// This works well for single-column documents.
    TopToBottomLeftToRight,

    /// Column-aware reading order.
    ///
    /// Uses the XY-Cut algorithm to detect columns and determines proper reading order
    /// across multiple columns. This works better for multi-column documents.
    ColumnAware,

    /// Structure tree first, with fallback to column-aware.
    ///
    /// For Tagged PDFs: Uses the PDF logical structure tree (ISO 32000-1:2008 Section 14.7)
    /// to determine reading order via Marked Content IDs (MCIDs). This is the PDF-spec-compliant
    /// approach and provides perfect reading order for Tagged PDFs.
    ///
    /// For Untagged PDFs: Falls back to ColumnAware (XY-Cut algorithm).
    ///
    /// This mode requires passing MCID reading order through ConversionOptions.mcid_order.
    StructureTreeFirst {
        /// Reading order as a sequence of MCIDs from structure tree traversal.
        /// If empty, falls back to ColumnAware mode.
        mcid_order: Vec<u32>,
    },
}
