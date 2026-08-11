use super::*;

#[test]
fn test_log_level_default() {
    assert_eq!(LogLevel::default(), LogLevel::Info);
}

#[test]
fn test_log_level_should_log() {
    let info_level = LogLevel::Info;
    assert!(info_level.should_log(LogLevel::Error));
    assert!(info_level.should_log(LogLevel::Warn));
    assert!(info_level.should_log(LogLevel::Info));
    assert!(!info_level.should_log(LogLevel::Debug));
    assert!(!info_level.should_log(LogLevel::Trace));

    let debug_level = LogLevel::Debug;
    assert!(debug_level.should_log(LogLevel::Error));
    assert!(debug_level.should_log(LogLevel::Warn));
    assert!(debug_level.should_log(LogLevel::Info));
    assert!(debug_level.should_log(LogLevel::Debug));
    assert!(!debug_level.should_log(LogLevel::Trace));

    let trace_level = LogLevel::Trace;
    assert!(trace_level.should_log(LogLevel::Error));
    assert!(trace_level.should_log(LogLevel::Warn));
    assert!(trace_level.should_log(LogLevel::Info));
    assert!(trace_level.should_log(LogLevel::Debug));
    assert!(trace_level.should_log(LogLevel::Trace));
}

#[test]
fn test_config_log_level_default() {
    let config = TextPipelineConfig::default();
    assert_eq!(config.log_level, LogLevel::Info);
}

#[test]
fn test_config_with_log_level() {
    let config = TextPipelineConfig::default().with_log_level(LogLevel::Debug);
    assert_eq!(config.log_level, LogLevel::Debug);
}

#[test]
fn test_config_with_log_level_trace() {
    let config = TextPipelineConfig::default().with_log_level(LogLevel::Trace);
    assert_eq!(config.log_level, LogLevel::Trace);
}

#[test]
fn test_config_with_log_level_error() {
    let config = TextPipelineConfig::default().with_log_level(LogLevel::Error);
    assert_eq!(config.log_level, LogLevel::Error);
}

#[test]
fn test_pdfplumber_compatible_has_log_level() {
    let config = TextPipelineConfig::pdfplumber_compatible();
    assert_eq!(config.log_level, LogLevel::Info);
}

// Document type preset tests

#[test]
fn test_document_type_academic_config() {
    let config = DocumentType::Academic.create_config();
    assert!(config.enable_hyphenation_reconstruction);
    assert_eq!(config.log_level, LogLevel::Info);
    assert!(config.output.preserve_layout);
    assert!(config.output.extract_tables);
    assert!(config.tj_threshold.use_adaptive);
}

#[test]
fn test_document_type_business_config() {
    let config = DocumentType::Business.create_config();
    assert!(config.enable_hyphenation_reconstruction);
    assert_eq!(config.log_level, LogLevel::Info);
    assert!(config.output.preserve_layout);
    assert!(config.output.extract_tables);
    assert_eq!(
        config.reading_order.strategy,
        ReadingOrderStrategyType::XYCut
    );
}

#[test]
fn test_document_type_novel_config() {
    let config = DocumentType::Novel.create_config();
    assert!(config.enable_hyphenation_reconstruction);
    assert!(!config.output.preserve_layout);
    assert!(!config.output.extract_tables);
    assert_eq!(
        config.reading_order.strategy,
        ReadingOrderStrategyType::Simple
    );
    assert!(!config.tj_threshold.use_adaptive);
}

#[test]
fn test_document_type_cjk_config() {
    let config = DocumentType::Cjk.create_config();
    assert!(!config.enable_hyphenation_reconstruction);
    assert_eq!(config.log_level, LogLevel::Info);
    assert!(config.output.preserve_layout);
    assert!(config.output.extract_tables);
    assert!(config.tj_threshold.use_adaptive);
    assert_eq!(config.word_boundary_mode, WordBoundaryMode::Primary);
}

#[test]
fn test_document_type_rtl_config() {
    let config = DocumentType::Rtl.create_config();
    assert!(!config.enable_hyphenation_reconstruction);
    assert!(config.output.preserve_layout);
    assert!(config.output.extract_tables);
    assert_eq!(config.word_boundary_mode, WordBoundaryMode::Tiebreaker);
}

#[test]
fn test_document_type_generic_config() {
    let config = DocumentType::Generic.create_config();
    // Generic should match the default config structure
    assert_eq!(config.log_level, LogLevel::default());
    assert_eq!(config.word_boundary_mode, WordBoundaryMode::default());
    assert!(config.enable_hyphenation_reconstruction);
}

// Document type detection tests

#[test]
fn test_detect_empty_sample() {
    let doc_type = DocumentType::detect_from_sample("");
    assert_eq!(doc_type, DocumentType::Generic);
}

#[test]
fn test_detect_cjk_sample() {
    let sample = "これは日本語です。This is bilingual text.";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Cjk);
}

#[test]
fn test_detect_cjk_chinese() {
    let sample = "这是中文文本。";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Cjk);
}

#[test]
fn test_detect_cjk_korean() {
    let sample = "이것은 한국어 텍스트입니다.";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Cjk);
}

#[test]
fn test_detect_rtl_sample() {
    let sample = "مرحبا بك في النص العربي";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Rtl);
}

#[test]
fn test_detect_rtl_hebrew() {
    let sample = "זה טקסט בעברית";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Rtl);
}

#[test]
fn test_detect_academic_sample() {
    // Sample has enough special chars for academic detection (>= 0.08 ratio)
    let sample =
        "The ∫∞√∑ equations © research shows ± evidence × mathematical ÷ concepts ® article";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Academic);
}

#[test]
fn test_detect_academic_with_symbols() {
    let sample = "Consider the integral ∫ from a to b and the summation ∑ with limit n → ∞ © 2024 ® ± × ÷ √ Author";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Academic);
}

#[test]
fn test_detect_novel_sample() {
    let sample = "The quick brown fox jumps over the lazy dog. She walked through the forest, listening to the birds singing their morning songs.";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Novel);
}

#[test]
fn test_detect_novel_narrative() {
    let sample = "Once upon a time, there was a kingdom far away. The princess walked through the castle gardens every morning, admiring the flowers and trees.";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Novel);
}

#[test]
fn test_detect_business_sample() {
    let sample = "Table 1 shows the results. Figure 2 displays the report findings. The document contains the agreement terms with key provisions.";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Business);
}

#[test]
fn test_detect_generic_mixed_text() {
    let sample =
        "ABC DEF GHI JKL MNO PQR STU VWX YZ are letters. Numbers like 1234567890 appear here too.";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Generic);
}

// Builder methods tests

#[test]
fn test_for_document_type_builder() {
    let config = TextPipelineConfig::for_document_type(DocumentType::Business);
    assert!(config.enable_hyphenation_reconstruction);
    assert!(config.output.extract_tables);
}

#[test]
fn test_detect_and_optimize() {
    let sample = "これは日本語です。";
    let config = TextPipelineConfig::detect_and_optimize(sample);
    assert_eq!(config.log_level, LogLevel::Info);
    assert!(!config.enable_hyphenation_reconstruction); // CJK config
}

#[test]
fn test_for_document_type_academic() {
    let config = TextPipelineConfig::for_document_type(DocumentType::Academic);
    assert!(config.tj_threshold.use_adaptive);
    assert_eq!(config.word_boundary_mode, WordBoundaryMode::Primary);
}

#[test]
fn test_for_document_type_cjk_spacing() {
    let config = TextPipelineConfig::for_document_type(DocumentType::Cjk);
    // CJK should have tighter spacing
    assert!(config.spacing.word_margin < 0.1);
}

#[test]
fn test_for_document_type_novel_spacing() {
    let config = TextPipelineConfig::for_document_type(DocumentType::Novel);
    // Novel should have relaxed spacing
    assert!(config.spacing.word_margin > 0.1);
}

#[test]
fn test_detect_sample_with_high_cjk_ratio() {
    let sample = "これはひらがなですあ。カタカナです。テスト。";
    let doc_type = DocumentType::detect_from_sample(sample);
    assert_eq!(doc_type, DocumentType::Cjk);
}

#[test]
fn test_detect_sample_with_low_cjk_ratio() {
    let sample = "This is mostly English text with some 日本語 mixed in.";
    let doc_type = DocumentType::detect_from_sample(sample);
    // Should be Generic since CJK ratio is too low
    assert_ne!(doc_type, DocumentType::Cjk);
}

#[test]
fn test_count_cjk_chars() {
    let text = "これは日本語です";
    let count = DocumentType::count_cjk_chars(text);
    assert!(count > 0);
}

#[test]
fn test_count_rtl_chars() {
    let text = "مرحبا بك";
    let count = DocumentType::count_rtl_chars(text);
    assert!(count > 0);
}

#[test]
fn test_count_special_chars() {
    let text = "Equation: ∫√∞ with © symbol";
    let count = DocumentType::count_special_chars(text);
    assert!(count > 0);
}

#[test]
fn test_looks_like_narrative() {
    let text =
        "she was running through the forest. she walked past the trees. they said hello to her.";
    assert!(DocumentType::looks_like_narrative(text));
}

#[test]
fn test_looks_not_like_narrative_high_digits() {
    let text = "1234567890 ABC DEF GHIJ 1234567890 KLMN";
    assert!(!DocumentType::looks_like_narrative(text));
}

#[test]
fn test_looks_like_business() {
    let text = "This Table shows the Figure in our report and document with agreement details";
    assert!(DocumentType::looks_like_business(text));
}

#[test]
fn test_looks_not_like_business() {
    let text = "This is a simple story about a dog and a cat in the forest";
    assert!(!DocumentType::looks_like_business(text));
}

// Metrics collection tests

#[test]
fn test_collect_metrics_default_disabled() {
    let config = TextPipelineConfig::default();
    assert!(!config.collect_metrics);
}

#[test]
fn test_collect_metrics_enabled() {
    let config = TextPipelineConfig::default().with_metrics_collection(true);
    assert!(config.collect_metrics);
}

#[test]
fn test_collect_metrics_disabled_explicitly() {
    let config = TextPipelineConfig::default().with_metrics_collection(false);
    assert!(!config.collect_metrics);
}

#[test]
fn test_collect_metrics_builder_chain() {
    let config = TextPipelineConfig::default()
        .with_log_level(LogLevel::Debug)
        .with_metrics_collection(true);
    assert!(config.collect_metrics);
    assert_eq!(config.log_level, LogLevel::Debug);
}
