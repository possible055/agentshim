use super::*;

/// Source of a space decision in the unified pipeline.
///
/// This enum tracks why a space was inserted (or not), which helps with
/// debugging and understanding the text extraction behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceSource {
    /// Space triggered by TJ offset value (negative offset > threshold)
    /// Confidence: 0.95 (explicit PDF positioning signal)
    TjOffset,

    /// Space triggered by geometric gap between spans
    /// Confidence: 0.8 (heuristic based on font metrics)
    GeometricGap,

    /// Space triggered by character transition heuristic (e.g., CamelCase, number->letter)
    /// Confidence: 0.6 (pattern-based heuristic)
    CharacterHeuristic,

    /// Space already present in boundary (no insertion needed)
    /// Confidence: 1.0 (deterministic)
    AlreadyPresent,

    /// No space inserted
    /// Confidence: varies (default when no rule matches)
    NoSpace,

    /// No space: suppressed specifically by the intra-word kerning guard
    /// (a lowercase↔lowercase gap below 0.75× the space-glyph advance). Kept
    /// distinct from `NoSpace` so the #847 per-line bimodal rescue can override
    /// ONLY this purely-geometric suppression, never the semantic ones
    /// (complex-script, CJK, ligature) that also return no-space.
    IntraWordKerning,

    /// Space triggered by WordBoundaryDetector analysis
    /// Confidence: 0.85 (combines TJ offset, geometric, and CJK signals per PDF Spec 9.4.4)
    WordBoundaryAnalysis,

    /// No space: a word split across two lines by a typesetter's hyphen.
    /// Kept distinct from `NoSpace` so the merge site can drop the hyphen itself,
    /// which is a rendering artefact rather than part of the word. Every other
    /// no-space rule joins spans verbatim.
    SoftHyphen,
}

/// Result of unified space decision process.
///
/// This struct is the single source of truth for whether a space should be inserted
/// between two text spans. It combines all available signals:
/// - TJ offset values from PDF content stream
/// - Geometric gaps between spans
/// - Character transition heuristics
/// - Existing boundary whitespace
///
/// Per PDF Spec ISO 32000-1:2008, Section 9.4.4 NOTE 6:
/// "The identification of what constitutes a word is unrelated to how the text
/// happens to be grouped into show strings... text strings should be as long as possible."
#[derive(Debug, Clone)]
pub struct SpaceDecision {
    /// Whether a space should be inserted
    pub insert_space: bool,

    /// Source/reason for this decision
    pub source: SpaceSource,

    /// Confidence score (0.0-1.0) indicating certainty
    pub confidence: f32,
}

impl SpaceDecision {
    /// Create a decision to insert a space from a specific source.
    pub fn insert(source: SpaceSource, confidence: f32) -> Self {
        Self {
            insert_space: true,
            source,
            confidence: confidence.clamp(0.0, 1.0),
        }
    }

    /// Create a decision to not insert a space.
    pub fn no_space(source: SpaceSource, confidence: f32) -> Self {
        Self {
            insert_space: false,
            source,
            confidence: confidence.clamp(0.0, 1.0),
        }
    }
}

/// Configuration for text extraction heuristics.
///
/// PDF spec does not define explicit rules for many spacing scenarios.
/// These configurable thresholds allow tuning extraction behavior.
///
/// # PDF Spec Reference
///
/// ISO 32000-1:2008, Section 9.4.4 - Text Positioning operators (TJ, Tj)
/// The spec defines how positioning works but NOT when a position offset
/// represents a word boundary vs. tight kerning.
#[derive(Debug, Clone)]
pub struct TextExtractionConfig {
    /// Extraction profile with document-type-specific thresholds
    ///
    /// When set, this profile overrides individual threshold settings and provides
    /// pre-tuned parameters optimized for specific document types.
    ///
    /// **Default**: None (uses legacy individual thresholds for backward compatibility)
    pub profile: Option<ExtractionProfile>,

    /// Threshold for inserting space characters in TJ arrays.
    ///
    /// **DEPRECATED**: Consider using `profile` with an `ExtractionProfile` or
    /// `word_margin_ratio` with `use_adaptive_tj_threshold` enabled for geometry-based
    /// adaptive thresholds. This field is used as a fallback when font metrics are
    /// unavailable or adaptive thresholds are disabled, and when profile is not set.
    ///
    /// **HEURISTIC**: When a TJ array contains a negative offset (in text space units),
    /// and that offset exceeds this threshold, a space character is inserted.
    ///
    /// **Default**: -120.0 units ≈ 0.12em
    /// - Typical word space: 0.25-0.33em (250-330 units)
    /// - Typical letter kerning: <0.1em (<100 units)
    ///
    /// **Lower values** (e.g., -80): More sensitive, inserts more spaces (may add spurious spaces)
    /// **Higher values** (e.g., -200): Less sensitive, inserts fewer spaces (may miss word boundaries)
    ///
    /// Set to `f32::NEG_INFINITY` to disable space insertion entirely.
    pub space_insertion_threshold: f32,

    /// Word margin ratio for geometry-based adaptive TJ threshold.
    ///
    /// When `use_adaptive_tj_threshold` is true and font metrics are available,
    /// the TJ offset threshold is calculated as:
    /// ```text
    /// adaptive_threshold = -(average_glyph_width * word_margin_ratio)
    /// ```
    ///
    /// This approach adapts to different font sizes and families by using the
    /// actual glyph metrics instead of a static value. This matches pdfplumber's
    /// `word_margin` parameter (default 0.1).
    ///
    /// **Default**: 0.1 (10% of average glyph width)
    ///
    /// **Typical values**:
    /// - 0.05: Tighter spacing (fewer spaces inserted, better for narrow fonts)
    /// - 0.1: Standard word spacing (default, matches pdfplumber)
    /// - 0.15: Looser spacing (more spaces inserted, better for wide fonts)
    ///
    /// **Note**: If font metrics are unavailable, falls back to `space_insertion_threshold`.
    ///
    /// # PDF Spec Reference
    ///
    /// ISO 32000-1:2008, Section 9.4.4 - TJ offsets are in thousandths of em.
    /// Average glyph width is also in thousandths of em, making this ratio
    /// dimensionally correct.
    pub word_margin_ratio: f32,

    /// Enable adaptive TJ threshold based on font geometry.
    ///
    /// When true, uses font metrics to calculate the TJ offset threshold dynamically:
    /// `adaptive_threshold = -(average_glyph_width * word_margin_ratio)`
    ///
    /// This replaces the static `space_insertion_threshold` with a value that adapts
    /// to different font sizes, families, and document layouts.
    ///
    /// **Default**: true (adaptive approach enabled)
    ///
    /// Set to `false` for backward compatibility with legacy behavior, which
    /// uses only the static `space_insertion_threshold`.
    ///
    /// # Benefits
    ///
    /// - Handles font size variations (8pt vs 24pt documents)
    /// - Adapts to different character widths (serif vs sans-serif, monospace vs proportional)
    /// - Reduces spurious spaces in policy documents with tight kerning
    /// - Maintains word boundary detection in academic documents
    pub use_adaptive_tj_threshold: bool,

    /// Word boundary detection mode for TJ array processing
    ///
    /// Controls whether WordBoundaryDetector is used as:
    /// - Tiebreaker: Only when TJ and geometric signals conflict (default)
    /// - Primary: Before creating TextSpans from tj_character_array
    ///
    /// **Default**: WordBoundaryMode::Tiebreaker (backward compatible)
    pub word_boundary_mode: WordBoundaryMode,
}

impl Default for TextExtractionConfig {
    fn default() -> Self {
        Self {
            profile: None,
            // Default -120.0 (conservative; matches existing
            // ExtractionProfile::CONSERVATIVE for byte-identical
            // back-compat). Callers handling TJ-heavy PDFs that
            // produce `Loremipsumdolorsitamet`-style merged
            // paragraphs can override via
            // `TextExtractionConfig::with_space_threshold(-100.0)` or
            // via the `TJ_HEAVY` extraction profile (see
            // config/extraction_profiles.rs). The default stays at
            // -120 to preserve byte-identical fixture output for the
            // 75-PDF regression sweep.
            //
            // Per-document calibration via gap_statistics is the
            // ideal root-cause fix; it requires a calibration corpus
            // to validate the threshold against without regressing
            // other inputs.
            space_insertion_threshold: -120.0,
            word_margin_ratio: 0.1,
            use_adaptive_tj_threshold: false,
            word_boundary_mode: WordBoundaryMode::default(),
        }
    }
}

impl TextExtractionConfig {
    /// Create a new configuration with default values.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::TextExtractionConfig;
    ///
    /// let config = TextExtractionConfig::new();
    /// assert_eq!(config.space_insertion_threshold, -120.0);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a configuration with custom space insertion threshold.
    ///
    /// # Arguments
    ///
    /// * `threshold` - Negative offset threshold for space insertion (in text space units)
    ///
    /// **Note**: This uses the static threshold. For better results, consider using
    /// `with_word_margin_ratio()` with adaptive thresholds enabled.
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::TextExtractionConfig;
    ///
    /// // More aggressive space insertion
    /// let config = TextExtractionConfig::with_space_threshold(-80.0);
    ///
    /// // Disable space insertion entirely
    /// let no_spaces = TextExtractionConfig::with_space_threshold(f32::NEG_INFINITY);
    /// ```
    pub fn with_space_threshold(threshold: f32) -> Self {
        Self {
            profile: None,
            space_insertion_threshold: threshold,
            word_margin_ratio: 0.1,
            use_adaptive_tj_threshold: false, // Static threshold mode
            word_boundary_mode: WordBoundaryMode::default(),
        }
    }

    /// Create a configuration with custom word margin ratio for adaptive TJ thresholds.
    ///
    /// # Arguments
    ///
    /// * `ratio` - Word margin ratio as fraction of average glyph width (typically 0.05-0.15)
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::TextExtractionConfig;
    ///
    /// // Standard adaptive thresholds (matches pdfplumber)
    /// let config = TextExtractionConfig::with_word_margin_ratio(0.1);
    ///
    /// // More aggressive (wider thresholds, more spaces)
    /// let aggressive = TextExtractionConfig::with_word_margin_ratio(0.15);
    ///
    /// // More conservative (narrower thresholds, fewer spaces)
    /// let conservative = TextExtractionConfig::with_word_margin_ratio(0.05);
    /// ```
    pub fn with_word_margin_ratio(ratio: f32) -> Self {
        Self {
            profile: None,
            space_insertion_threshold: -120.0, // Fallback value
            word_margin_ratio: ratio,
            use_adaptive_tj_threshold: true, // Adaptive threshold mode
            word_boundary_mode: WordBoundaryMode::default(),
        }
    }

    /// Set the word margin ratio on an existing configuration (builder pattern).
    ///
    /// # Arguments
    ///
    /// * `ratio` - Word margin ratio as fraction of average glyph width
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::TextExtractionConfig;
    ///
    /// let config = TextExtractionConfig::new()
    ///     .set_word_margin_ratio(0.15);
    /// ```
    pub fn set_word_margin_ratio(mut self, ratio: f32) -> Self {
        self.word_margin_ratio = ratio;
        self.use_adaptive_tj_threshold = true;
        self
    }

    /// Enable or disable adaptive TJ thresholds (builder pattern).
    ///
    /// # Arguments
    ///
    /// * `enabled` - Whether to use adaptive thresholds based on font metrics
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::TextExtractionConfig;
    ///
    /// // Use static threshold only
    /// let config = TextExtractionConfig::new()
    ///     .set_adaptive_tj_threshold(false);
    /// ```
    pub fn set_adaptive_tj_threshold(mut self, enabled: bool) -> Self {
        self.use_adaptive_tj_threshold = enabled;
        self
    }

    /// Set the extraction profile and apply its threshold configuration (builder pattern).
    ///
    /// This applies the profile's thresholds to the configuration, selecting document-type-specific
    /// parameters for better text extraction quality.
    ///
    /// # Arguments
    ///
    /// * `profile` - An extraction profile (e.g., ACADEMIC, POLICY, FORM)
    ///
    /// # Examples
    ///
    /// ```
    /// use pdf_oxide::extractors::TextExtractionConfig;
    /// use pdf_oxide::config::ExtractionProfile;
    ///
    /// // Use ACADEMIC profile for research papers
    /// let config = TextExtractionConfig::new()
    ///     .with_profile(ExtractionProfile::ACADEMIC);
    /// ```
    pub fn with_profile(mut self, profile: ExtractionProfile) -> Self {
        // Extract profile settings before moving profile
        let tj_offset = profile.tj_offset_threshold;
        let word_margin = profile.word_margin_ratio;
        let use_adaptive = profile.use_adaptive_threshold;

        // Set profile and apply its thresholds
        self.profile = Some(profile);
        self.space_insertion_threshold = tj_offset;
        self.word_margin_ratio = word_margin;
        self.use_adaptive_tj_threshold = use_adaptive;
        self
    }
}
