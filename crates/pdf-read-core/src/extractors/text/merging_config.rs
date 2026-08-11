use super::*;

/// Configuration for span merging behavior.
///
/// These thresholds control how adjacent text spans are merged together and when
/// spaces are inserted between them. All thresholds are in PDF points (1/72 inch).
///
/// # Rationale
///
/// PDF content streams don't explicitly mark word boundaries - text can be rendered
/// with arbitrary gaps. These configurable thresholds allow tuning extraction to
/// different document types:
/// - Academic papers: tight column spacing, small gaps between words
/// - Documents with tables: larger gaps to preserve structure
/// - Dense grids (author lists): very small gaps that are still word boundaries
///
/// # References
///
/// Typography standards: word spacing typically 0.25-0.33em (25-33% of font size)
/// See: SPAN_SPACING_INVESTIGATION.md for empirical measurements
#[derive(Clone, Debug, PartialEq)]
pub struct SpanMergingConfig {
    /// Minimum gap (in multiples of font size) to trigger space insertion.
    ///
    /// When the gap between two spans exceeds this threshold, a space is inserted.
    /// Expressed as a ratio of font size (em).
    ///
    /// **Default**: 0.25
    /// - Based on typography standards: typical word spacing is 0.25-0.33em
    /// - For 12pt font: 0.25em * 12pt = 3pt
    /// - For 10pt font: 0.25em * 10pt = 2.5pt
    ///
    /// **Tuning guidance**:
    /// - Lower values (0.15-0.20): More aggressive space insertion, catches dense layouts
    /// - Higher values (0.33-0.50): Conservative, only clear word boundaries
    pub space_threshold_em_ratio: f32,

    /// Conservative threshold for font transitions (in points).
    ///
    /// Below this gap, don't insert a space even if gap > 0, to avoid spurious spaces
    /// from font metric changes or very tight kerning.
    ///
    /// **Default**: 0.1
    /// - Avoids spaces from font metric alignment issues (very tight threshold)
    /// - Smaller than typical letter spacing in justified text
    /// - Catches actual overlaps/reversals while preserving character adjacency
    ///
    /// **Note**: Changed from 0.3 to 0.1 after regression testing revealed
    /// that 0.3pt was too conservative for policy documents (0.1-0.3pt word spacing),
    /// causing word fusion. Adaptive threshold analysis recommended for future improvement.
    ///
    /// **Tuning guidance**:
    /// - Lower values (0.1-0.2): More aggressive, inserts more spaces
    /// - Higher values (0.5-1.0): Conservative, only clear separations
    pub conservative_threshold_pt: f32,

    /// Column boundary threshold (in points).
    ///
    /// Gaps larger than this indicate column separation and prevent span merging.
    /// Used to preserve document structure (e.g., multi-column layouts, tables).
    ///
    /// **Default**: 5.0
    /// - Typical character width for 10-12pt font: 4-6pt
    /// - Word spacing: 2-4pt
    /// - Column gaps in academic papers: 5-15pt
    /// - Table column gaps: 10-50pt
    ///
    /// **Tuning guidance**:
    /// - Lower values (3.0-4.0): Merge more spans, risk merging across columns
    /// - Higher values (8.0-10.0): Keep columns separate, preserve structure
    pub column_boundary_threshold_pt: f32,

    /// Negative gap threshold for severe overlaps (in points).
    ///
    /// When gaps are negative (spans overlap), values more severe than this
    /// indicate genuine overlap and should prevent merging.
    ///
    /// **Default**: -0.5
    /// - Typical font metric variations: 0 to -0.3pt
    /// - Small overlaps from kerning: -0.3 to -0.5pt
    /// - Real overlap errors: worse than -0.5pt
    ///
    /// **Tuning guidance**:
    /// - Less negative (-0.2, -0.1): More conservative on overlaps
    /// - More negative (-1.0, -2.0): Allow some overlap to merge adjacent text
    pub severe_overlap_threshold_pt: f32,

    /// Enable adaptive threshold analysis (default: true).
    ///
    /// When true, the `conservative_threshold_pt` is automatically calculated
    /// based on the gap distribution within the document. This overrides the fixed
    /// threshold value and adapts to different document types.
    ///
    /// **Default**: true (adaptive enabled)
    /// Enabled by default to improve extraction quality across document types.
    /// Use `SpanMergingConfig::legacy()` for the old fixed-threshold behavior.
    ///
    /// # Performance
    ///
    /// Adaptive analysis adds minimal overhead (O(n log n) for gap analysis where n = spans).
    /// Expected overhead: <5% of total extraction time.
    pub use_adaptive_threshold: bool,

    /// Configuration for adaptive threshold analysis.
    ///
    /// Only used when `use_adaptive_threshold` is true.
    /// If None, uses `AdaptiveThresholdConfig::default()`.
    ///
    /// Allows fine-tuning the adaptive analysis for specific document types:
    /// - `AdaptiveThresholdConfig::policy_documents()` - For tight spacing
    /// - `AdaptiveThresholdConfig::academic()` - For standard spacing
    /// - `AdaptiveThresholdConfig::aggressive()` - For dense layouts
    /// - `AdaptiveThresholdConfig::conservative()` - For formal documents
    pub adaptive_config: Option<crate::extractors::gap_statistics::AdaptiveThresholdConfig>,

    /// Enable email pattern detection for spacing decisions.
    ///
    /// When true, detects email-like patterns in surrounding text
    /// (e.g., "user@domain" separated by spaces) and applies special spacing rules
    /// to preserve email addresses.
    ///
    /// Per PDF Spec ISO 32000-1:2008 Section 9.10, only extracted text patterns
    /// are used - no domain-specific semantics.
    ///
    /// **Default**: false
    pub detect_email_patterns: bool,

    /// Multiplier for email pattern threshold detection.
    ///
    /// Controls how aggressively email patterns are detected by adjusting the gap threshold.
    /// A multiplier > 1.0 makes detection more lenient (allows larger gaps to be considered email context).
    /// A multiplier < 1.0 makes detection stricter.
    ///
    /// Calculated as: `email_threshold = geometric_threshold * email_threshold_multiplier`
    ///
    /// **Default**: 2.5
    /// - At 2.5×, handles typical email address separations with spaces
    /// - Typical gap between email parts: 4-8pt (after @, before TLD)
    pub email_threshold_multiplier: f32,

    /// Enable citation marker detection for spacing decisions.
    ///
    /// When true, detects superscript citation markers (typically smaller font size)
    /// and adjusts spacing rules to preserve citation formatting.
    ///
    /// Per PDF Spec ISO 32000-1:2008 Section 9.10, font size ratios from extracted content
    /// are used for detection.
    ///
    /// **Default**: false
    pub detect_citation_markers: bool,

    /// Font size ratio for citation marker detection.
    ///
    /// Citation markers typically have font size between this ratio and 1.0 of the base text.
    /// Values below this ratio are considered citation markers.
    ///
    /// **Default**: 0.75
    /// - Typical citation markers: 70-80% of text font size
    /// - Superscript usually: 50-80% of base font
    pub citation_font_size_ratio: f32,

    /// When `false`, each `Tm` operator starts a fresh span regardless of position.
    /// Use this to preserve column boundaries for callers that need per-positioned-run spans
    /// (e.g. pdftotext `-bbox-layout` parity).
    ///
    /// # Warning
    /// Disabling this on character-by-character-positioned PDFs (common in academic typesetting)
    /// can produce very large span counts per page (100× or more).
    ///
    /// Default: `true` (existing behaviour preserved).
    ///
    /// Reference: ISO 32000-1 §9.4.2 / §9.4.4 NOTE 6.
    pub merge_tm_tj_runs: bool,
}

impl Default for SpanMergingConfig {
    fn default() -> Self {
        Self {
            space_threshold_em_ratio: 0.25,
            conservative_threshold_pt: 0.1, // Reverted from 0.3 after regression testing
            column_boundary_threshold_pt: 5.0,
            severe_overlap_threshold_pt: -0.5,
            use_adaptive_threshold: true, // Enabled by default for better quality
            adaptive_config: None,
            detect_email_patterns: false,
            email_threshold_multiplier: 2.5,
            detect_citation_markers: false,
            citation_font_size_ratio: 0.75,
            merge_tm_tj_runs: true,
        }
    }
}

impl SpanMergingConfig {
    /// Create a new configuration with default values.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::SpanMergingConfig;
    ///
    /// let config = SpanMergingConfig::new();
    /// assert_eq!(config.space_threshold_em_ratio, 0.25);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a configuration with aggressive space insertion (for dense layouts).
    ///
    /// Uses lower thresholds to insert spaces more readily:
    /// - space_threshold_em_ratio: 0.15 (instead of 0.25)
    /// - conservative_threshold_pt: 0.1 (instead of 0.3)
    ///
    /// Good for documents with many short words close together (author lists, grids).
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::SpanMergingConfig;
    ///
    /// let config = SpanMergingConfig::aggressive();
    /// ```
    pub fn aggressive() -> Self {
        Self {
            space_threshold_em_ratio: 0.15,
            conservative_threshold_pt: 0.1,
            column_boundary_threshold_pt: 5.0,
            severe_overlap_threshold_pt: -0.5,
            use_adaptive_threshold: false,
            adaptive_config: None,
            detect_email_patterns: false,
            email_threshold_multiplier: 2.5,
            detect_citation_markers: false,
            citation_font_size_ratio: 0.75,
            merge_tm_tj_runs: true,
        }
    }

    /// Create a configuration with conservative space insertion (for formal documents).
    ///
    /// Uses higher thresholds to insert spaces less readily:
    /// - space_threshold_em_ratio: 0.33 (instead of 0.25)
    /// - conservative_threshold_pt: 0.3 (instead of 0.1)
    ///
    /// Good for formal documents where spacing is reliable.
    ///
    /// **Note**: After regression testing, 0.5pt threshold was found to cause
    /// excessive word fusion in policy documents. Reduced to 0.3pt.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::SpanMergingConfig;
    ///
    /// let config = SpanMergingConfig::conservative();
    /// ```
    pub fn conservative() -> Self {
        Self {
            space_threshold_em_ratio: 0.33,
            conservative_threshold_pt: 0.3, // Reduced from 0.5 (was too aggressive for policy docs)
            column_boundary_threshold_pt: 5.0,
            severe_overlap_threshold_pt: -0.5,
            use_adaptive_threshold: false,
            adaptive_config: None,
            detect_email_patterns: false,
            email_threshold_multiplier: 2.5,
            detect_citation_markers: false,
            citation_font_size_ratio: 0.75,
            merge_tm_tj_runs: true,
        }
    }

    /// Create a configuration with custom thresholds.
    ///
    /// # Arguments
    ///
    /// * `space_threshold_em` - Space threshold as em ratio
    /// * `conservative_pt` - Conservative gap threshold in points
    /// * `column_boundary_pt` - Column boundary threshold in points
    /// * `overlap_pt` - Severe overlap threshold in points
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::SpanMergingConfig;
    ///
    /// let config = SpanMergingConfig::custom(0.2, 0.2, 6.0, -0.3);
    /// ```
    pub fn custom(
        space_threshold_em: f32,
        conservative_pt: f32,
        column_boundary_pt: f32,
        overlap_pt: f32,
    ) -> Self {
        Self {
            space_threshold_em_ratio: space_threshold_em,
            conservative_threshold_pt: conservative_pt,
            column_boundary_threshold_pt: column_boundary_pt,
            severe_overlap_threshold_pt: overlap_pt,
            use_adaptive_threshold: false,
            adaptive_config: None,
            detect_email_patterns: false,
            email_threshold_multiplier: 2.5,
            detect_citation_markers: false,
            citation_font_size_ratio: 0.75,
            merge_tm_tj_runs: true,
        }
    }

    /// Create a configuration with adaptive threshold enabled (default settings).
    ///
    /// This enables automatic threshold calculation based on the document's gap
    /// distribution. Uses conservative base settings for reliable defaults:
    /// - space_threshold_em_ratio: 0.25
    /// - conservative_threshold_pt: 0.1 (overridden by adaptive calculation)
    /// - column_boundary_threshold_pt: 5.0
    /// - severe_overlap_threshold_pt: -0.5
    /// - adaptive_config: AdaptiveThresholdConfig::default()
    ///
    /// The adaptive threshold is computed as: median_gap * 1.5, clamped to [0.05, 1.0] points.
    ///
    /// # Benefits
    ///
    /// - Automatically adapts to different document types
    /// - Reduces word fusion in policy documents with tight spacing
    /// - Minimizes spurious spaces in other document types
    /// - Maintains backward compatibility (disabled by default)
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::SpanMergingConfig;
    ///
    /// let config = SpanMergingConfig::adaptive();
    /// assert!(config.use_adaptive_threshold);
    /// ```
    pub fn adaptive() -> Self {
        Self {
            space_threshold_em_ratio: 0.25,
            conservative_threshold_pt: 0.1,
            column_boundary_threshold_pt: 5.0,
            severe_overlap_threshold_pt: -0.5,
            use_adaptive_threshold: true,
            adaptive_config: Some(
                crate::extractors::gap_statistics::AdaptiveThresholdConfig::default(),
            ),
            detect_email_patterns: false,
            email_threshold_multiplier: 2.5,
            detect_citation_markers: false,
            citation_font_size_ratio: 0.75,
            merge_tm_tj_runs: true,
        }
    }

    /// Create a configuration with adaptive threshold and custom settings.
    ///
    /// # Arguments
    ///
    /// * `adaptive_config` - Custom adaptive threshold configuration
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::{SpanMergingConfig, AdaptiveThresholdConfig};
    ///
    /// let config = SpanMergingConfig::adaptive_with_config(
    ///     AdaptiveThresholdConfig::policy_documents()
    /// );
    /// assert!(config.use_adaptive_threshold);
    /// ```
    pub fn adaptive_with_config(
        adaptive_config: crate::extractors::gap_statistics::AdaptiveThresholdConfig,
    ) -> Self {
        Self {
            space_threshold_em_ratio: 0.25,
            conservative_threshold_pt: 0.1,
            column_boundary_threshold_pt: 5.0,
            severe_overlap_threshold_pt: -0.5,
            use_adaptive_threshold: true,
            adaptive_config: Some(adaptive_config),
            detect_email_patterns: false,
            email_threshold_multiplier: 2.5,
            detect_citation_markers: false,
            citation_font_size_ratio: 0.75,
            merge_tm_tj_runs: true,
        }
    }

    /// Create a configuration using the legacy fixed-threshold approach.
    ///
    /// This provides backward compatibility with legacy behavior where
    /// adaptive threshold was disabled by default. All thresholds are fixed values.
    ///
    /// **Default values**:
    /// - space_threshold_em_ratio: 0.25 (standard word spacing)
    /// - conservative_threshold_pt: 0.1 (tight font metric threshold)
    /// - column_boundary_threshold_pt: 5.0 (standard column separation)
    /// - severe_overlap_threshold_pt: -0.5 (standard overlap tolerance)
    /// - use_adaptive_threshold: false (no automatic adjustment)
    ///
    /// # When to Use
    ///
    /// Use this when you need the fixed-threshold behavior:
    /// - Testing regression against old baselines
    /// - Documents with known quirks that required specific thresholds
    /// - Performance-critical applications where adaptive overhead is unacceptable
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::SpanMergingConfig;
    ///
    /// let config = SpanMergingConfig::legacy();
    /// assert!(!config.use_adaptive_threshold);
    /// assert_eq!(config.conservative_threshold_pt, 0.1);
    /// ```
    pub fn legacy() -> Self {
        Self {
            space_threshold_em_ratio: 0.25,
            conservative_threshold_pt: 0.1,
            column_boundary_threshold_pt: 5.0,
            severe_overlap_threshold_pt: -0.5,
            use_adaptive_threshold: false, // Fixed thresholds, no adaptive
            adaptive_config: None,
            detect_email_patterns: false,
            email_threshold_multiplier: 2.5,
            detect_citation_markers: false,
            citation_font_size_ratio: 0.75,
            merge_tm_tj_runs: true,
        }
    }
}
