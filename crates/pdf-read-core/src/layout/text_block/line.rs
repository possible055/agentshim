use super::*;

impl TextLine {
    /// Create a new TextLine from a list of words.
    ///
    /// # Panics
    ///
    /// Panics if the `words` vector is empty.
    pub fn new(words: Vec<Word>) -> Self {
        assert!(!words.is_empty(), "Cannot create TextLine from empty words");

        // Compute bounding box as union of all word bboxes
        let bbox = words
            .iter()
            .map(|w| w.bbox)
            .fold(words[0].bbox, |acc, r| acc.union(&r));

        // Join word text with spaces
        let text = words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");

        Self { words, bbox, text }
    }
}
