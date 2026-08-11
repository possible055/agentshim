use super::*;

mod adaptive_thresholds;
mod profiles;

/// Compact TextSpan builder for the intervening-ink decimal tests.
fn digit_test_span(text: &str, bbox: Rect, font_size: f32) -> TextSpan {
    TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: text.to_string(),
        bbox,
        font_name: "F1".to_string(),
        font_size,
        font_weight: FontWeight::Normal,
        color: Color::black(),
        mcid: None,
        mcid_scope: None,
        sequence: 0,
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
