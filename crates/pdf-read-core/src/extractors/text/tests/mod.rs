use super::*;
use crate::fonts::{Encoding, LazyCMap};
use std::sync::Arc;

mod cjk_and_rtl;
mod decoding_and_fonts;
mod gap_models;
mod glyph_positioning;
mod marked_content;
mod operators_and_state;
mod spacing_boundaries;
mod spacing_core;
mod span_merging;
mod span_ordering;
mod text_matrices;
mod word_boundaries;

fn create_test_font() -> FontInfo {
    FontInfo {
        base_font: "Times-Roman".to_string(),
        subtype: "Type1".to_string(),
        encoding: Encoding::Standard("WinAnsiEncoding".to_string()),
        to_unicode: None,
        font_weight: None,
        flags: None,
        stem_v: None,
        ascent: 0.95,
        descent: -0.35,
        embedded_font_data: None,
        truetype_cmap: std::sync::OnceLock::new(),
        embedded_glyph_names: std::sync::OnceLock::new(),
        is_truetype_font: false,
        widths: None,
        first_char: None,
        last_char: None,
        font_matrix_a: 0.001,
        default_width: 1000.0,
        cid_to_gid_map: None,
        cid_system_info: None,
        cid_font_type: None,
        cid_widths: None,
        cid_default_width: 1000.0,
        has_explicit_dw: false,
        cff_gid_map: None,
        multi_char_map: HashMap::new(),
        byte_to_char_table: std::sync::OnceLock::new(),
        type0_unicode_memo: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        byte_to_width_table: std::sync::OnceLock::new(),
        weight_memo: std::sync::OnceLock::new(),
        italic_memo: std::sync::OnceLock::new(),
        std14_memo: std::sync::OnceLock::new(),
        diff_glyph_names: std::collections::HashMap::new(),
        wmode: 0,
        cid_vertical_metrics: None,
        cid_default_vertical_metrics: crate::fonts::VerticalMetrics::SPEC_DEFAULT,
        cjk_substitution: None,
    }
}

// #575: snap_superscript_baselines was O(n²) (every span scanned against
// every other), hanging >30 s on archive.org/Google-Books pages whose
// invisible hOCR layer emits tens of thousands of spans. The Y-windowed
// rewrite must (a) still snap a superscript onto its base and (b) scale —
// 50k spans take ~10-20 s under the old double loop but milliseconds now,
// so a generous wall-clock bound catches a quadratic regression without
// being flaky.
fn snap_span(text: &str, x: f32, y: f32, w: f32, fs: f32, seq: usize) -> TextSpan {
    TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: text.to_string(),
        bbox: Rect::new(x, y, w, fs),
        font_name: "F1".to_string(),
        font_size: fs,
        font_weight: FontWeight::Normal,
        color: Color::black(),
        mcid: None,
        mcid_scope: None,
        sequence: seq,
        split_boundary_before: false,
        offset_semantic: false,
        is_italic: false,
        is_monospace: false,
        char_spacing: 0.0,
        word_spacing: 0.0,
        horizontal_scaling: 100.0,
        primary_detected: false,
        char_widths: vec![],
        char_x_offsets: Vec::new(),
        heading_level: None,
        rotation_degrees: 0.0,
        wmode: 0,
        rtl_draw_logical: false,
    }
}
