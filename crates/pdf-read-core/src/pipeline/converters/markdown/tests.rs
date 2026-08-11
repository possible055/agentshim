use super::*;
use crate::geometry::Rect;
use crate::layout::{Color, TextSpan};
use crate::pipeline::converters::span_in_table;
use crate::pipeline::StructRole;
use crate::structure::table_extractor::{TableCell, TableRow};

fn make_span_w(
    text: &str,
    x: f32,
    y: f32,
    width: f32,
    font_size: f32,
    weight: FontWeight,
) -> OrderedTextSpan {
    OrderedTextSpan::new(
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: text.to_string(),
            bbox: Rect::new(x, y, width, font_size),
            font_name: "Test".to_string(),
            font_size,
            font_weight: weight,
            is_italic: false,
            is_monospace: false,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 0,
            offset_semantic: false,
            split_boundary_before: false,
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
        },
        0,
    )
}

fn make_span(text: &str, x: f32, y: f32, font_size: f32, weight: FontWeight) -> OrderedTextSpan {
    OrderedTextSpan::new(
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: text.to_string(),
            bbox: Rect::new(x, y, 50.0, font_size),
            font_name: "Test".to_string(),
            font_size,
            font_weight: weight,
            is_italic: false,
            is_monospace: false,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 0,
            offset_semantic: false,
            split_boundary_before: false,
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
        },
        0,
    )
}

fn config_with_headings() -> TextPipelineConfig {
    let mut config = TextPipelineConfig::default();
    config.output.detect_headings = true;
    config
}

/// Helper to create a span with a specific width (for gap-detection tests).
fn make_span_with_width(
    text: &str,
    x: f32,
    y: f32,
    width: f32,
    font_size: f32,
    weight: FontWeight,
    order: usize,
) -> OrderedTextSpan {
    let mut s = OrderedTextSpan::new(
        TextSpan {
            provenance: None,
            text_rise: 0.0,
            artifact_type: None,
            text: text.to_string(),
            bbox: Rect::new(x, y, width, font_size),
            font_name: "Test".to_string(),
            font_size,
            font_weight: weight,
            is_italic: false,
            is_monospace: false,
            color: Color::black(),
            mcid: None,
            mcid_scope: None,
            sequence: 0,
            offset_semantic: false,
            split_boundary_before: false,
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
        },
        order,
    );
    s.reading_order = order;
    s
}

mod basics_a;
mod basics_b;
mod layout_and_tables;
mod regressions_a;
mod regressions_b;
