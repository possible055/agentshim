//! Text and content extraction from PDF documents.
//!
//! Provides high-performance extraction of text, images, paths, and layout analysis.

pub mod auto;
pub mod ccitt_bilevel;
pub mod forms;
pub mod gap_statistics;
pub mod geometric_spacing;
pub mod images;
pub mod paths;
pub mod pattern_detector;
pub mod text;
pub mod warnings;

pub use auto::PageKind;
pub use forms::{FieldType, FieldValue, FormExtractor, FormField};
pub use gap_statistics::{
    analyze_document_gaps, calculate_statistics, determine_adaptive_threshold, extract_gaps,
    AdaptiveThresholdConfig, AdaptiveThresholdResult, GapStatistics,
};
pub use geometric_spacing::{should_insert_space, SpaceInsertion, SpacingConfig};
pub use images::{
    expand_inline_image_dict, extract_image_from_xobject, ColorSpace, ImageData, PdfImage,
    PixelFormat,
};
pub use paths::{FillRule, PathExtractor};
pub use pattern_detector::{PatternDetector, PatternPreservationConfig};
pub use text::{SpanMergingConfig, TextExtractionConfig, TextExtractor};
pub use warnings::{Warning, WarningCategory, WarningSink};
