use super::*;

impl TextBlock {
    /// Create a text block from a collection of characters.
    ///
    /// This computes the bounding box, text content, average font size,
    /// and dominant font from the character data.
    ///
    /// # Panics
    ///
    /// Panics if the `chars` vector is empty.
    pub fn from_chars(chars: Vec<TextChar>) -> Self {
        assert!(
            !chars.is_empty(),
            "Cannot create TextBlock from empty chars"
        );

        // Collect text directly
        let text: String = chars.iter().map(|c| c.char).collect();

        // Compute bounding box as union of all character bboxes
        let bbox = chars
            .iter()
            .map(|c| c.bbox)
            .fold(chars[0].bbox, |acc, r| acc.union(&r));

        let avg_font_size = chars.iter().map(|c| c.font_size).sum::<f32>() / chars.len() as f32;

        // Find dominant font (most common)
        let mut font_counts = HashMap::new();
        for c in &chars {
            *font_counts.entry(c.font_name.clone()).or_insert(0) += 1;
        }
        let dominant_font = font_counts
            .iter()
            .max_by_key(|(_, count)| *count)
            .map(|(font, _)| font.clone())
            .unwrap_or_default();

        let is_bold = chars.iter().any(|c| c.font_weight.is_bold());
        let is_italic = chars.iter().any(|c| c.is_italic);

        // Determine MCID for the block
        let mcid = chars
            .first()
            .and_then(|c| c.mcid)
            .filter(|&first_mcid| chars.iter().all(|c| c.mcid == Some(first_mcid)));

        // A word's glyphs share one text-rendering matrix in practice; take
        // the rotation all chars agree on, and fall back to upright when a
        // block mixes rotations (no single frame describes it).
        let rotation_degrees = chars
            .first()
            .map(|c| c.rotation_degrees)
            .filter(|&r| chars.iter().all(|c| c.rotation_degrees == r))
            .unwrap_or(0.0);

        Self {
            chars,
            bbox,
            text,
            avg_font_size,
            dominant_font,
            is_bold,
            is_italic,
            mcid,
            sequence: 0,
            rotation_degrees,
        }
    }

    /// Get the center point of the text block.
    pub fn center(&self) -> Point {
        self.bbox.center()
    }

    /// Check if this block is horizontally aligned with another block.
    pub fn is_horizontally_aligned(&self, other: &TextBlock, tolerance: f32) -> bool {
        (self.bbox.y - other.bbox.y).abs() < tolerance
    }

    /// Check if this block is vertically aligned with another block.
    pub fn is_vertically_aligned(&self, other: &TextBlock, tolerance: f32) -> bool {
        (self.bbox.x - other.bbox.x).abs() < tolerance
    }
}
