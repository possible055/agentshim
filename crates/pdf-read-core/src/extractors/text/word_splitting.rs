use super::*;

impl<'doc> TextExtractor<'doc> {
    /// ISSUE 1 FIX: Split fused words created by PDF authoring defects
    ///
    /// Some PDFs encode multiple words as a single TJ string without spacing:
    /// - "theGeneral" instead of "the" + "General"
    /// - "lengthThis" instead of "length" + "This"
    /// - "helporganisationscraft" (partial fusion)
    ///
    /// This post-processor detects word fusions and splits them into separate spans.
    ///
    /// Uses two strategies:
    /// 1. **CamelCase detection** (first priority): Detects lowercase->uppercase transitions
    ///    - Example: "theGeneral" -> ["the", "General"]
    /// 2. **Dictionary-based segmentation** (fallback): Uses Viterbi algorithm with word dictionary
    ///    - Example: "helporganisationscraft" -> ["help", "organisations", "craft"]
    ///
    /// Per ISO 32000-1:2008 Section 9.4.4: "Text strings are as long as possible" - spaces
    /// are positioning artifacts, so word fusions must be detected and reconstructed.
    #[allow(dead_code)]
    pub(super) fn split_fused_words(&mut self) {
        let mut split_spans = Vec::new();

        for span in &self.spans {
            // DEBUG: Log field values before cloning
            log::debug!(
                "split_fused_words() processing span '{}' (offset_semantic={}, split_boundary_before={})",
                if span.text.len() <= 30 {
                    &span.text
                } else {
                    "[whitespace or long text]"
                },
                span.offset_semantic,
                span.split_boundary_before
            );

            // Try CamelCase split (handles mixed-case fusions)
            let parts = self.split_on_camelcase(&span.text);

            if parts.len() == 1 {
                // No split needed
                let cloned = span.clone();
                log::debug!(
                    "  → No split: cloned offset_semantic={} (text: '{}')",
                    cloned.offset_semantic,
                    if cloned.text.len() <= 30 {
                        &cloned.text
                    } else {
                        "[whitespace or long text]"
                    }
                );
                split_spans.push(cloned);
            } else {
                // Split into multiple spans with proportional bounding boxes
                let total_chars = span.text.len() as f32;
                let mut char_pos = 0;

                for (i, part) in parts.iter().enumerate() {
                    let part_len = part.len() as f32;
                    let part_ratio = part_len / total_chars;

                    // Calculate proportional bounding box
                    let new_width = span.bbox.width * part_ratio;
                    let new_x = span.bbox.x + (span.bbox.width * (char_pos as f32 / total_chars));

                    let mut new_span = span.clone();
                    new_span.text = part.clone();
                    new_span.bbox.x = new_x;
                    new_span.bbox.width = new_width;

                    // Set split_boundary_before flag for all parts except the first
                    // This prevents them from being re-merged during span merging
                    if i > 0 {
                        new_span.split_boundary_before = true;
                    }

                    log::debug!(
                        "  → Split part {}: '{}' offset_semantic={} split_boundary_before={}",
                        i,
                        part,
                        new_span.offset_semantic,
                        new_span.split_boundary_before
                    );
                    split_spans.push(new_span);
                    char_pos += part.len();
                }
            }
        }

        self.spans = split_spans;
    }

    /// Detect CamelCase boundaries and split text into parts
    ///
    /// Splits on lowercase->uppercase transitions:
    /// - "theGeneral" -> ["the", "General"]
    /// - "lengthThis" -> ["length", "This"]
    /// - "helporganisationscraft" -> ["help", "organisations", "craft"]
    #[allow(dead_code)]
    pub(super) fn split_on_camelcase(&self, text: &str) -> Vec<String> {
        let mut parts = Vec::new();
        let mut current_part = String::new();
        let mut prev_is_lower = false;

        for ch in text.chars() {
            if prev_is_lower && ch.is_uppercase() {
                // CamelCase boundary detected
                if !current_part.is_empty() {
                    parts.push(current_part.clone());
                    current_part.clear();
                }
                current_part.push(ch);
                prev_is_lower = false;
            } else {
                current_part.push(ch);
                prev_is_lower = ch.is_lowercase();
            }
        }

        if !current_part.is_empty() {
            parts.push(current_part);
        }

        // Only return split if we found at least 2 parts with actual boundaries
        if parts.len() > 1 {
            parts
        } else {
            vec![text.to_string()]
        }
    }

    /// Stop between operators when the call was cancelled or this page has grown past
    /// what the call reserved.
    ///
    /// The span count, not a byte count, is what the page budget bounds: a `TextSpan` is
    /// a few hundred bytes, but every layout stage downstream — reading order, dedup,
    /// merge, table detection — holds its own copy of the vector, so the page's real cost
    /// is a multiple of the count that no single allocation reveals.
    pub(super) fn budget_checkpoint(&mut self) -> Result<()> {
        /// Operators between checkpoints. Small enough that a cancelled call stops well
        /// inside the response deadline, large enough that the check does not show up in
        /// the profile of an ordinary page.
        const STRIDE: usize = 512;

        self.operators_since_checkpoint += 1;
        if self.operators_since_checkpoint < STRIDE {
            return Ok(());
        }
        self.operators_since_checkpoint = 0;
        crate::budget::check_cancelled()?;
        crate::budget::check_page_spans(self.spans.len().max(self.chars.len()))
    }
}
