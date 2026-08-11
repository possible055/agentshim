use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Extract widget annotation values as TextSpans positioned at their /Rect locations.
    ///
    /// Converts each widget annotation's field value into a `TextSpan` with the annotation's
    /// bounding box. These spans merge naturally with content stream spans and get positioned
    /// correctly by existing layout algorithms.
    pub(super) fn extract_widget_spans(&self, page_index: usize) -> Vec<TextSpan> {
        use crate::extractors::forms::field_flags;
        use crate::geometry::Rect;

        let page_obj = match self.get_page(page_index) {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };
        let page_dict = match page_obj.as_dict() {
            Some(d) => d,
            None => return Vec::new(),
        };

        // Get /Annots array (may be direct or indirect)
        let annots_arr = match page_dict.get("Annots") {
            Some(Object::Array(arr)) => arr.clone(),
            Some(Object::Reference(r)) => match self.load_object(*r) {
                Ok(Object::Array(arr)) => arr,
                _ => return Vec::new(),
            },
            _ => return Vec::new(),
        };

        let mut spans = Vec::new();
        let base_sequence = 1_000_000; // high sequence number so widget spans sort after content spans at same Y

        for (idx, annot_obj) in annots_arr.iter().enumerate() {
            let annot_ref = match annot_obj {
                Object::Reference(r) => *r,
                _ => continue,
            };
            let dict = match self.load_object(annot_ref) {
                Ok(obj) => match obj.as_dict() {
                    Some(d) => d.clone(),
                    None => continue,
                },
                Err(_) => continue,
            };

            // Only process Widget annotations
            let subtype = match dict.get("Subtype").and_then(|s| s.as_name()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if !subtype.eq_ignore_ascii_case("widget") {
                continue;
            }

            // Check /F flags — skip invisible/hidden/noview annotations
            // Bit 1 (0x1) = Invisible, Bit 2 (0x2) = Hidden, Bit 6 (0x20) = NoView
            if let Some(Object::Integer(f)) = dict.get("F") {
                if *f & (0x1 | 0x2 | 0x20) != 0 {
                    continue;
                }
            }

            // Parse /Rect [x1, y1, x2, y2] → Rect { x, y, width, height }
            let rect = match dict.get("Rect") {
                Some(Object::Array(arr)) if arr.len() == 4 => {
                    let mut coords = [0.0f32; 4];
                    let mut ok = true;
                    for (i, item) in arr.iter().enumerate() {
                        match item {
                            Object::Integer(n) => coords[i] = *n as f32,
                            Object::Real(f) => coords[i] = *f as f32,
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if !ok {
                        continue;
                    }
                    let x = coords[0].min(coords[2]);
                    let y = coords[1].min(coords[3]);
                    let w = (coords[2] - coords[0]).abs();
                    let h = (coords[3] - coords[1]).abs();
                    if w < 0.1 || h < 0.1 {
                        continue;
                    } // skip zero-area rects
                    Rect::new(x, y, w, h)
                }
                Some(Object::Reference(r)) => match self.load_object(*r) {
                    Ok(Object::Array(arr)) if arr.len() == 4 => {
                        let mut coords = [0.0f32; 4];
                        let mut ok = true;
                        for (i, item) in arr.iter().enumerate() {
                            match item {
                                Object::Integer(n) => coords[i] = *n as f32,
                                Object::Real(f) => coords[i] = *f as f32,
                                _ => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if !ok {
                            continue;
                        }
                        let x = coords[0].min(coords[2]);
                        let y = coords[1].min(coords[3]);
                        let w = (coords[2] - coords[0]).abs();
                        let h = (coords[3] - coords[1]).abs();
                        if w < 0.1 || h < 0.1 {
                            continue;
                        }
                        Rect::new(x, y, w, h)
                    }
                    _ => continue,
                },
                _ => continue,
            };

            // Get field type via /FT (with parent-chain inheritance)
            let ft = dict
                .get("FT")
                .and_then(|o| o.as_name())
                .map(|s| s.to_string())
                .or_else(|| self.resolve_inherited_ft(&dict));

            // Get field flags /Ff (with parent-chain inheritance)
            let ff = dict
                .get("Ff")
                .and_then(|o| match o {
                    Object::Integer(i) => Some(*i as u32),
                    _ => None,
                })
                .or_else(|| self.resolve_inherited_ff(&dict));
            let ff = ff.unwrap_or(0);

            // Determine display text based on field type
            let display_text = match ft.as_deref() {
                Some("Tx") => {
                    // Text field: use /V string value
                    if ff & field_flags::PASSWORD != 0 {
                        // Password field: render as asterisks
                        Some("********".to_string())
                    } else {
                        let value = Self::parse_string_value_static(dict.get("V"))
                            .or_else(|| self.resolve_inherited_field_value(&dict));
                        match value {
                            Some(v) if !v.trim().is_empty() => {
                                // Bound the value to the widget's visual
                                // capacity. Multi-line text-area fields
                                // can hold scrollable content far larger
                                // than the bbox visually renders; per
                                // spec §12.7.4.3 `/V` is the field's
                                // data, but `extract_text` semantics
                                // target what would be visible on the
                                // page. Truncate keeps the rendered
                                // portion and drops the overflow.
                                Some(Self::truncate_to_widget_capacity(
                                    v.trim().to_string(),
                                    &rect,
                                ))
                            }
                            _ => {
                                // Fallback: try AP stream text. Truncate
                                // to bbox capacity — some PDFs reuse a
                                // single Form XObject for many widgets'
                                // `/AP /N`, pointing every widget's
                                // appearance at the page-background
                                // content; without the cap each widget
                                // would extract that content once.
                                self.extract_text_from_ap_stream(&dict).and_then(|t| {
                                    let t = t.trim().to_string();
                                    if t.is_empty() {
                                        return None;
                                    }
                                    Some(Self::truncate_to_widget_capacity(t, &rect))
                                })
                            }
                        }
                    }
                }
                Some("Btn") => {
                    if ff & field_flags::PUSH_BUTTON != 0 {
                        // Push button: caption is in /MK /CA per PDF Spec
                        // ISO 32000-1:2008 §12.5.6.19 (Appearance Characteristics
                        // Dictionary). Extracting it lets screen readers
                        // text-extraction consumers see the button label.
                        dict.get("MK")
                            .and_then(|mk| mk.as_dict())
                            .and_then(|mk| Self::parse_string_value_static(mk.get("CA")))
                            .and_then(|s| {
                                let t = s.trim().to_string();
                                if t.is_empty() {
                                    None
                                } else {
                                    Some(t)
                                }
                            })
                    } else {
                        // Checkbox or radio button
                        let value = Self::parse_string_value_static(dict.get("V"))
                            .or_else(|| self.resolve_inherited_field_value(&dict));
                        let is_checked = match &value {
                            Some(v) => {
                                let v_lower = v.to_ascii_lowercase();
                                v_lower != "off" && !v_lower.is_empty()
                            }
                            None => false,
                        };
                        if is_checked {
                            // A checked box is meaningful state worth surfacing.
                            Some("[x]".to_string())
                        } else {
                            // An UNCHECKED box carries no text. Emitting "[ ]"
                            // here injected noise that pdftotext/PyMuPDF never
                            // produce — the dominant cause of pdf_oxide being
                            // the sole outlier on AcroForm-heavy PDFs in the
                            // cross-corpus sweep (CORPUS-1). Emit nothing.
                            None
                        }
                    }
                }
                Some("Ch") => {
                    // Choice field: use /V selected value
                    let value = dict.get("V");
                    match value {
                        Some(Object::Array(arr)) => {
                            // Multiple selections: join with ", "
                            let items: Vec<String> = arr
                                .iter()
                                .filter_map(|item| Self::parse_string_value_static(Some(item)))
                                .collect();
                            if items.is_empty() {
                                None
                            } else {
                                Some(items.join(", "))
                            }
                        }
                        other => Self::parse_string_value_static(other)
                            .or_else(|| self.resolve_inherited_field_value(&dict))
                            .and_then(|v| {
                                let t = v.trim().to_string();
                                if t.is_empty() {
                                    None
                                } else {
                                    Some(t)
                                }
                            }),
                    }
                }
                Some("Sig") => {
                    // Signature field: skip (no user-visible text)
                    None
                }
                _ => {
                    // Unknown field type: try /V as text
                    Self::parse_string_value_static(dict.get("V"))
                        .or_else(|| self.resolve_inherited_field_value(&dict))
                        .and_then(|v| {
                            let t = v.trim().to_string();
                            if t.is_empty() {
                                None
                            } else {
                                Some(t)
                            }
                        })
                }
            };

            let text = match display_text {
                Some(t) if !t.is_empty() => t,
                _ => {
                    // CORPUS-5: a widget with no extractable /V value (notably a
                    // signature field, /FT /Sig) often carries its VISIBLE text
                    // in the /AP/N appearance stream (e.g. "Firmato
                    // elettronicamente da ..."). pdftotext / PyMuPDF surface it;
                    // fall back to the appearance stream so it isn't dropped.
                    // Fields that DO yield a /V value take the arm above, so this
                    // never double-extracts.
                    match self.extract_text_from_ap_stream(&dict) {
                        Some(ap) if !ap.trim().is_empty() => ap.trim().to_string(),
                        _ => continue,
                    }
                }
            };

            // Parse font size from /DA string
            let font_size = {
                let da = dict
                    .get("DA")
                    .and_then(|o| match o {
                        Object::String(s) => Some(Self::decode_pdf_text_string(s)),
                        _ => None,
                    })
                    .or_else(|| self.resolve_inherited_da(&dict));

                match da {
                    Some(da_str) => {
                        let size = Self::parse_font_size_from_da(&da_str);
                        if size <= 0.0 {
                            // Auto-size: estimate from rect height
                            (rect.height * 0.7).clamp(6.0, 24.0)
                        } else {
                            size
                        }
                    }
                    None => {
                        // No DA at all: estimate from rect height
                        (rect.height * 0.7).clamp(6.0, 24.0)
                    }
                }
            };

            spans.push(TextSpan {
                provenance: None,
                artifact_type: None,
                text,
                bbox: rect,
                font_name: String::new(),
                font_size,
                font_weight: crate::layout::text_block::FontWeight::Normal,
                is_italic: false,
                is_monospace: false,
                color: crate::layout::text_block::Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                },
                mcid: None,
                mcid_scope: None,
                sequence: base_sequence + idx,
                split_boundary_before: false,
                offset_semantic: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                text_rise: 0.0,
                rtl_draw_logical: false,
            });
        }

        spans
    }

    /// Build TextSpan objects from the /Contents field of content-bearing annotations.
    ///
    /// Sticky note (/Subtype/Text), FreeText, Stamp, and markup annotations carry
    /// human-readable text in their /Contents field. Widget annotations are already
    /// handled by `extract_widget_spans`; Popup annotations hold no independent
    /// content (their text belongs to the parent annotation).
    pub(super) fn annotation_content_spans(&self, page_index: usize) -> Vec<TextSpan> {
        use crate::geometry::Rect;

        let page_obj = match self.get_page(page_index) {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };
        let page_dict = match page_obj.as_dict() {
            Some(d) => d,
            None => return Vec::new(),
        };

        let annots_arr = match page_dict.get("Annots") {
            Some(Object::Array(arr)) => arr.clone(),
            Some(Object::Reference(r)) => match self.load_object(*r) {
                Ok(Object::Array(arr)) => arr,
                _ => return Vec::new(),
            },
            _ => return Vec::new(),
        };

        let mut spans: Vec<TextSpan> = Vec::new();
        let base_sequence = 2_000_000usize; // sort after widget spans

        for (idx, annot_obj) in annots_arr.iter().enumerate() {
            let annot_ref = match annot_obj {
                Object::Reference(r) => *r,
                _ => continue,
            };
            let dict = match self.load_object(annot_ref) {
                Ok(obj) => match obj.as_dict() {
                    Some(d) => d.clone(),
                    None => continue,
                },
                Err(_) => continue,
            };

            let subtype = match dict.get("Subtype").and_then(|s| s.as_name()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let subtype_lc = subtype.to_ascii_lowercase();

            // Skip Widget (handled by extract_widget_spans) and Popup (no independent content).
            if subtype_lc == "widget" || subtype_lc == "popup" {
                continue;
            }

            // Skip invisible / hidden / NoView annotations.
            if let Some(Object::Integer(f)) = dict.get("F") {
                if *f & (0x1 | 0x2 | 0x20) != 0 {
                    continue;
                }
            }

            // Only FreeText and Stamp have /Contents representing visible page text.
            // Text (sticky-note) /Contents is reviewer comment text shown in a pop-up
            // window, not rendered on the page — exclude it to avoid injecting popup
            // notes into the body text stream.
            // For FreeText/Stamp: try /Contents first; fall back to AP stream so that
            // Stamp annotations with empty /Contents but a rendered AP stream are included.
            let is_visible = matches!(subtype_lc.as_str(), "freetext" | "stamp");
            if !is_visible {
                continue;
            }

            let text = {
                let from_contents = if let Some(Object::String(s)) = dict.get("Contents") {
                    let decoded = Self::decode_pdf_text_string(s).trim().to_string();
                    if decoded.is_empty() {
                        None
                    } else {
                        Some(decoded)
                    }
                } else {
                    None
                };
                if let Some(t) = from_contents {
                    t
                } else {
                    match self.extract_text_from_ap_stream(&dict) {
                        Some(ap_text) if !ap_text.trim().is_empty() => ap_text.trim().to_string(),
                        _ => continue,
                    }
                }
            };

            // Use /Rect as the annotation's bounding box.
            // /Rect may be a direct array or an indirect reference to an array.
            let rect_obj = match dict.get("Rect") {
                Some(Object::Reference(r)) => match self.load_object(*r) {
                    Ok(o) => o,
                    Err(_) => continue,
                },
                Some(o) => o.clone(),
                None => continue,
            };
            let rect = match rect_obj.as_array() {
                Some(arr) if arr.len() == 4 => {
                    let mut coords = [0.0f32; 4];
                    let mut ok = true;
                    for (i, item) in arr.iter().enumerate() {
                        match item {
                            Object::Integer(n) => coords[i] = *n as f32,
                            Object::Real(f) => coords[i] = *f as f32,
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if !ok {
                        continue;
                    }
                    let x = coords[0].min(coords[2]);
                    let y = coords[1].min(coords[3]);
                    let w = (coords[2] - coords[0]).abs();
                    let h = (coords[3] - coords[1]).abs();
                    Rect {
                        x,
                        y,
                        width: w.max(1.0),
                        height: h.max(1.0),
                    }
                }
                _ => continue,
            };

            spans.push(TextSpan {
                provenance: None,
                artifact_type: None,
                text,
                bbox: rect,
                font_name: String::new(),
                font_size: 12.0,
                font_weight: crate::layout::text_block::FontWeight::Normal,
                is_italic: false,
                is_monospace: false,
                color: crate::layout::text_block::Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                },
                mcid: None,
                mcid_scope: None,
                sequence: base_sequence + idx,
                split_boundary_before: false,
                offset_semantic: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 100.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                text_rise: 0.0,
                rtl_draw_logical: false,
            });
        }

        spans
    }

    /// Walk /Parent chain to find inherited /Ff (field flags) value.
    pub(super) fn resolve_inherited_ff(
        &self,
        dict: &std::collections::HashMap<String, Object>,
    ) -> Option<u32> {
        let mut parent_ref = match dict.get("Parent") {
            Some(Object::Reference(r)) => Some(*r),
            _ => return None,
        };
        let mut depth = 0;
        while let Some(pref) = parent_ref {
            if depth >= 10 {
                break;
            }
            depth += 1;
            if let Ok(parent_obj) = self.load_object(pref) {
                if let Some(parent_dict) = parent_obj.as_dict() {
                    if let Some(Object::Integer(ff)) = parent_dict.get("Ff") {
                        return Some(*ff as u32);
                    }
                    parent_ref = match parent_dict.get("Parent") {
                        Some(Object::Reference(r)) => Some(*r),
                        _ => None,
                    };
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        None
    }

    /// Walk /Parent chain (and AcroForm) to find inherited /DA (Default Appearance) string.
    pub(super) fn resolve_inherited_da(
        &self,
        dict: &std::collections::HashMap<String, Object>,
    ) -> Option<String> {
        // First check parent chain
        let mut parent_ref = match dict.get("Parent") {
            Some(Object::Reference(r)) => Some(*r),
            _ => None,
        };
        let mut depth = 0;
        while let Some(pref) = parent_ref {
            if depth >= 10 {
                break;
            }
            depth += 1;
            if let Ok(parent_obj) = self.load_object(pref) {
                if let Some(parent_dict) = parent_obj.as_dict() {
                    if let Some(Object::String(da)) = parent_dict.get("DA") {
                        return Some(Self::decode_pdf_text_string(da));
                    }
                    parent_ref = match parent_dict.get("Parent") {
                        Some(Object::Reference(r)) => Some(*r),
                        _ => None,
                    };
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        // Fall back to AcroForm-level /DA
        if let Some(trailer_dict) = self.trailer.as_dict() {
            if let Some(root_ref) = trailer_dict.get("Root").and_then(|o| o.as_reference()) {
                if let Ok(root_obj) = self.load_object(root_ref) {
                    if let Some(root_dict) = root_obj.as_dict() {
                        let acroform = match root_dict.get("AcroForm") {
                            Some(Object::Reference(r)) => self.load_object(*r).ok(),
                            Some(obj) => Some(obj.clone()),
                            None => None,
                        };
                        if let Some(acroform_obj) = acroform {
                            if let Some(af_dict) = acroform_obj.as_dict() {
                                if let Some(Object::String(da)) = af_dict.get("DA") {
                                    return Some(Self::decode_pdf_text_string(da));
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }
}
