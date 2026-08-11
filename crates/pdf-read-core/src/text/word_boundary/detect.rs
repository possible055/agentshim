use super::*;

/// Detect word boundaries in a character stream.
///
/// This is a convenience function that creates a detector with default settings
/// and performs boundary detection in one call.
///
/// # Arguments
///
/// * `characters` - Sequence of characters with positioning information
/// * `context` - Font metrics and text state parameters
///
/// # Returns
///
/// Vector of indices where word boundaries occur
pub fn detect_word_boundaries(
    characters: &[CharacterInfo],
    context: &BoundaryContext,
) -> Vec<usize> {
    let detector = WordBoundaryDetector::new();
    detector.detect_word_boundaries(characters, context)
}
