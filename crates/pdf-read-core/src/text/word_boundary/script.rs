use super::*;

impl DocumentScript {
    /// Detect document script profile by sampling first 1000 characters.
    ///
    /// This optimization reduces boundary detection overhead by skipping
    /// unnecessary script detection for documents with known script profiles.
    ///
    /// PERFORMANCE: O(min(n, 1000)) sampling, executed once per extraction
    pub fn detect_from_characters(characters: &[CharacterInfo]) -> Self {
        if characters.is_empty() {
            return Self::Latin; // Default to Latin for empty documents
        }

        let mut has_rtl = false;
        let mut has_cjk = false;
        let mut has_complex = false;
        let sample_size = characters.len().min(1000);

        // Sample first 1000 characters to classify document
        for ch in &characters[..sample_size] {
            // Check for RTL (fast range check)
            if (0x0590..=0x08FF).contains(&ch.code) || (0xFB1D..=0xFDFF).contains(&ch.code) {
                has_rtl = true;
            }

            // Check for CJK (fast range checks - common ranges first)
            if (0x4E00..=0x9FFF).contains(&ch.code) // Han
                || (0x3040..=0x309F).contains(&ch.code) // Hiragana
                || (0x30A0..=0x30FF).contains(&ch.code) // Katakana
                || (0xAC00..=0xD7AF).contains(&ch.code)
            {
                // Hangul
                has_cjk = true;
            }

            // Check for complex scripts. The Brahmic South-Asian blocks
            // (Bengali, Tamil, Telugu, Kannada, Malayalam) were previously
            // absent, so those docs classified as Latin/Mixed and never reached
            // the complex-script boundary rules — leaking spurious spaces after
            // matras (#656-class Indic gap). They share the same matra/virama
            // boundary semantics as Devanagari.
            if (0x0900..=0x097F).contains(&ch.code) // Devanagari
                || (0x0980..=0x09FF).contains(&ch.code) // Bengali
                || (0x0B80..=0x0BFF).contains(&ch.code) // Tamil
                || (0x0C00..=0x0C7F).contains(&ch.code) // Telugu
                || (0x0C80..=0x0CFF).contains(&ch.code) // Kannada
                || (0x0D00..=0x0D7F).contains(&ch.code) // Malayalam
                || (0x0E00..=0x0E7F).contains(&ch.code) // Thai
                || (0x1780..=0x17FF).contains(&ch.code)
            {
                // Khmer
                has_complex = true;
            }
        }

        // Decision tree: classify based on what we found
        #[allow(clippy::let_and_return)]
        let script = match (has_rtl, has_cjk, has_complex) {
            (false, false, false) => Self::Latin, // Pure Latin (fast path)
            (false, true, _) => Self::CJK,        // CJK-dominant (skip RTL)
            (true, false, _) => Self::RTL,        // RTL-dominant (skip CJK)
            (_, _, true) => Self::Complex,        // Complex scripts present
            _ => Self::Mixed,                     // Mixed scripts
        };

        // Log detected script at TRACE level for debugging
        log::trace!(
            "Detected document script: {:?} (sampled {} characters)",
            script,
            sample_size
        );

        script
    }
}
