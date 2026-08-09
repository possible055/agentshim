#[derive(Clone, Copy, Debug)]
pub struct AutoExtractOptions {
    pub min_text_confidence: Option<f32>,
}

impl AutoExtractOptions {
    pub fn balanced() -> Self {
        Self {
            min_text_confidence: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    TextLayer,
    Scanned,
    ImageText,
    Mixed,
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasonCode {
    Ok,
    NativeTextHighConfidence,
    NoTextLayerPresent,
    TextLayerBelowThreshold,
    GlyphMappingMissing,
    EncryptedNoExtractPermission,
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageCodecClass {
    None,
    Ccitt,
    Jbig2,
    Dct,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProducerPrior {
    Scanner,
    Authoring,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSignals {
    pub text_glyph_count: usize,
    pub text_area_ratio: f32,
    pub image_area_ratio: f32,
    pub codec: ImageCodecClass,
    pub invisible_text_ratio: f32,
    pub garbled_ratio: f32,
    pub fragmented_word_ratio: f32,
    pub consecutive_repeat_ratio: f32,
    pub vector_path_density: f32,
    pub has_reliable_structure: bool,
    pub producer_prior: ProducerPrior,
    pub page_is_empty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageClassification {
    pub page: usize,
    pub kind: PageKind,
    pub confidence: f32,
    pub reason: ReasonCode,
    pub signals: PageSignals,
}

#[must_use]
pub fn is_cjk_dominant_text(text: &str) -> bool {
    let mut total = 0_usize;
    let mut cjk = 0_usize;
    for character in text.chars().filter(|character| !character.is_whitespace()) {
        total += 1;
        let codepoint = character as u32;
        if (0x4E00..=0x9FFF).contains(&codepoint)
            || (0x3400..=0x4DBF).contains(&codepoint)
            || (0x3040..=0x309F).contains(&codepoint)
            || (0x30A0..=0x30FF).contains(&codepoint)
            || (0xAC00..=0xD7A3).contains(&codepoint)
        {
            cjk += 1;
        }
    }
    total > 0 && (cjk as f32 / total as f32) > 0.5
}

#[must_use]
pub fn text_quality_gate(text: &str) -> Option<ReasonCode> {
    let characters = text.chars().collect::<Vec<_>>();
    let total = characters.len();
    if total < 16 {
        return None;
    }
    let bad = characters
        .iter()
        .filter(|&&character| {
            character == '\u{FFFD}'
                || ('\u{0}'..'\u{9}').contains(&character)
                || ('\u{E}'..'\u{20}').contains(&character)
                || ('\u{E000}'..='\u{F8FF}').contains(&character)
        })
        .count();
    if bad as f32 / total as f32 > 0.20 {
        return Some(ReasonCode::GlyphMappingMissing);
    }

    let words = text.split_whitespace().collect::<Vec<_>>();
    if words.len() >= 8 {
        let fragmented = words
            .iter()
            .filter(|word| word.chars().count() <= 2)
            .count() as f32
            / words.len() as f32;
        let cjk_dominant = is_cjk_dominant_text(text);
        if fragmented > 0.80 && !cjk_dominant {
            return Some(ReasonCode::GlyphMappingMissing);
        }
        let repeated =
            words.windows(2).filter(|pair| pair[0] == pair[1]).count() as f32 / words.len() as f32;
        let average_length = words.iter().map(|word| word.chars().count()).sum::<usize>() as f32
            / words.len() as f32;
        if repeated > 0.30 || (fragmented > 0.55 && average_length < 2.5 && !cjk_dominant) {
            return Some(ReasonCode::TextLayerBelowThreshold);
        }
    }

    let alphanumeric = characters
        .iter()
        .filter(|character| character.is_alphanumeric())
        .count();
    if alphanumeric as f32 / (total as f32) < 0.20 {
        return Some(ReasonCode::TextLayerBelowThreshold);
    }
    None
}

#[must_use]
pub fn classify_from_signals(
    signals: &PageSignals,
    options: &AutoExtractOptions,
) -> (PageKind, f32, ReasonCode) {
    if signals.page_is_empty {
        return (PageKind::Empty, 0.99, ReasonCode::Empty);
    }

    let usable_text = signals.text_glyph_count >= 24
        && signals.garbled_ratio <= 0.20
        && signals.fragmented_word_ratio <= 0.80;
    if !usable_text && signals.vector_path_density > 0.60 && signals.image_area_ratio < 0.20 {
        return (PageKind::Scanned, 0.80, ReasonCode::NoTextLayerPresent);
    }
    if signals.image_area_ratio >= 0.80 {
        if usable_text && signals.invisible_text_ratio >= 0.50 {
            return (
                PageKind::TextLayer,
                0.85,
                ReasonCode::NativeTextHighConfidence,
            );
        }
        if signals.text_area_ratio < 0.10 || !usable_text {
            let confidence = if matches!(
                signals.codec,
                ImageCodecClass::Ccitt | ImageCodecClass::Jbig2
            ) {
                0.95
            } else {
                0.85
            };
            let reason = if usable_text {
                ReasonCode::TextLayerBelowThreshold
            } else {
                ReasonCode::NoTextLayerPresent
            };
            return (PageKind::Scanned, confidence, reason);
        }
    }
    if usable_text && signals.image_area_ratio > 0.05 && signals.image_area_ratio < 0.80 {
        return (PageKind::ImageText, 0.75, ReasonCode::Ok);
    }
    if usable_text {
        let mut confidence = options.min_text_confidence.unwrap_or(0.70).max(0.80);
        if signals.has_reliable_structure {
            confidence = (confidence + 0.10).min(0.99);
        }
        return (
            PageKind::TextLayer,
            confidence,
            ReasonCode::NativeTextHighConfidence,
        );
    }
    if signals.text_glyph_count > 0 {
        let clean = signals.garbled_ratio <= 0.20 && signals.fragmented_word_ratio <= 0.80;
        if clean && signals.image_area_ratio < 0.05 {
            return (
                PageKind::TextLayer,
                0.60,
                ReasonCode::NativeTextHighConfidence,
            );
        }
        return (PageKind::Scanned, 0.80, ReasonCode::GlyphMappingMissing);
    }
    let confidence = match signals.producer_prior {
        ProducerPrior::Scanner => 0.85,
        _ => 0.70,
    };
    (
        PageKind::Scanned,
        confidence,
        ReasonCode::NoTextLayerPresent,
    )
}
