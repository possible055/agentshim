//! AcroForm field extraction.
//!
//! Extracts form fields from PDF documents that use AcroForms (Interactive Forms).
//! See ISO 32000-1:2008, Section 12.7 - Interactive Forms.

use crate::document::PdfDocument;
use crate::error::{Error, Result};
use crate::object::{Object, ObjectRef};

/// A form field extracted from a PDF AcroForm.
#[derive(Debug, Clone)]
pub struct FormField {
    /// Field name from /T key
    pub name: String,
    /// Field type from /FT key
    pub field_type: FieldType,
    /// Field value from /V key
    pub value: FieldValue,
    /// Tooltip/description from /TU key
    pub tooltip: Option<String>,
    /// Full qualified name (for hierarchical fields)
    pub full_name: String,
    /// Field bounding box from /Rect key [x1, y1, x2, y2]
    pub bounds: Option<[f64; 4]>,

    // === Additional properties for modification support ===
    /// Object reference for updating existing fields
    pub object_ref: Option<ObjectRef>,
    /// Field flags from /Ff key (ReadOnly, Required, NoExport, etc.)
    pub flags: Option<u32>,
    /// Default value from /DV key
    pub default_value: Option<FieldValue>,
    /// Maximum length for text fields from /MaxLen key
    pub max_length: Option<u32>,
    /// Text alignment from /Q key (0=left, 1=center, 2=right)
    pub alignment: Option<u32>,
    /// Default appearance string from /DA key
    pub default_appearance: Option<String>,
    /// Border style from /BS key
    pub border_style: Option<BorderStyle>,
    /// Appearance characteristics from /MK key
    pub appearance_chars: Option<AppearanceCharacteristics>,
}

/// Field type from /FT key in field dictionary.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    /// Button field (/Btn) - checkbox, radio button, push button
    Button,
    /// Text field (/Tx) - single or multi-line text
    Text,
    /// Choice field (/Ch) - list box or combo box
    Choice,
    /// Signature field (/Sig)
    Signature,
    /// Unknown/unrecognized field type
    Unknown(String),
}

/// Field value from /V key in field dictionary.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    /// Text string value
    Text(String),
    /// Boolean value (for checkboxes)
    Boolean(bool),
    /// Name value (for radio buttons, choice fields)
    Name(String),
    /// Array of values (for multi-select list boxes)
    Array(Vec<String>),
    /// No value present
    None,
}

/// Border style from /BS dictionary (PDF Table 166).
#[derive(Debug, Clone, PartialEq)]
pub struct BorderStyle {
    /// Border width in points
    pub width: f32,
    /// Border style type
    pub style: BorderStyleType,
    /// Dash pattern for dashed borders
    pub dash_array: Option<Vec<u32>>,
}

impl Default for BorderStyle {
    fn default() -> Self {
        Self {
            width: 1.0,
            style: BorderStyleType::Solid,
            dash_array: None,
        }
    }
}

/// Border style type from /S key in /BS dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BorderStyleType {
    /// Solid border
    #[default]
    Solid,
    /// Dashed border
    Dashed,
    /// Beveled border (3D effect, raised)
    Beveled,
    /// Inset border (3D effect, recessed)
    Inset,
    /// Underline only
    Underline,
}

impl BorderStyleType {
    /// Parse from PDF name.
    pub fn from_pdf_name(name: &str) -> Self {
        match name {
            "S" => BorderStyleType::Solid,
            "D" => BorderStyleType::Dashed,
            "B" => BorderStyleType::Beveled,
            "I" => BorderStyleType::Inset,
            "U" => BorderStyleType::Underline,
            _ => BorderStyleType::Solid,
        }
    }

    /// Convert to PDF name.
    pub fn to_pdf_name(&self) -> &'static str {
        match self {
            BorderStyleType::Solid => "S",
            BorderStyleType::Dashed => "D",
            BorderStyleType::Beveled => "B",
            BorderStyleType::Inset => "I",
            BorderStyleType::Underline => "U",
        }
    }
}

/// Appearance characteristics from /MK dictionary (PDF Table 189).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppearanceCharacteristics {
    /// Background color (BG) - RGB values 0.0-1.0
    pub background_color: Option<[f32; 3]>,
    /// Border color (BC) - RGB values 0.0-1.0
    pub border_color: Option<[f32; 3]>,
    /// Normal caption (CA) - text shown on button
    pub caption: Option<String>,
    /// Rollover caption (RC) - text shown when hovering
    pub rollover_caption: Option<String>,
    /// Alternate caption (AC) - text shown when pressed
    pub alternate_caption: Option<String>,
    /// Rotation angle in degrees (R)
    pub rotation: Option<u32>,
}

/// Field flag constants from PDF Table 221.
pub mod field_flags {
    /// Field is read-only (bit 1)
    pub const READ_ONLY: u32 = 1;
    /// Field is required (bit 2)
    pub const REQUIRED: u32 = 1 << 1;
    /// Field should not be exported (bit 3)
    pub const NO_EXPORT: u32 = 1 << 2;

    // Text field flags (PDF Table 228)
    /// Text field allows multiple lines (bit 13)
    pub const MULTILINE: u32 = 1 << 12;
    /// Text field is a password field (bit 14)
    pub const PASSWORD: u32 = 1 << 13;
    /// Text field does not scroll (bit 24)
    pub const DO_NOT_SCROLL: u32 = 1 << 23;
    /// Text field allows comb formatting (bit 25)
    pub const COMB: u32 = 1 << 24;
    /// Text field is a rich text field (bit 26)
    pub const RICH_TEXT: u32 = 1 << 25;

    // Button field flags (PDF Table 226)
    /// Button is a push button (bit 17)
    pub const PUSH_BUTTON: u32 = 1 << 16;
    /// Radio button (bit 16)
    pub const RADIO: u32 = 1 << 15;
    /// Radio buttons in group are exclusive (bit 26)
    pub const RADIOS_IN_UNISON: u32 = 1 << 25;

    // Choice field flags (PDF Table 230)
    /// Choice is a combo box (bit 18)
    pub const COMBO: u32 = 1 << 17;
    /// Choice field is editable (bit 19)
    pub const EDIT: u32 = 1 << 18;
    /// Choice field is sorted (bit 20)
    pub const SORT: u32 = 1 << 19;
    /// Choice field allows multiple selection (bit 22)
    pub const MULTI_SELECT: u32 = 1 << 21;
    /// Do not spell check (bit 23)
    pub const DO_NOT_SPELL_CHECK: u32 = 1 << 22;
    /// Commit on change (bit 27)
    pub const COMMIT_ON_SEL_CHANGE: u32 = 1 << 26;
}

/// AcroForm extractor.
pub struct FormExtractor;

impl FormExtractor {
    /// Helper function to resolve an Object (handles indirect references).
    ///
    /// If the object is an indirect reference, loads it. Otherwise returns clone.
    fn resolve_object(doc: &PdfDocument, obj: &Object) -> Result<Object> {
        if let Some(ref_val) = obj.as_reference() {
            doc.load_object(ref_val)
        } else {
            Ok(obj.clone())
        }
    }

    /// Decode a PDF string that may be UTF-16BE (with BOM) or PDFDocEncoding.
    ///
    /// Per ISO 32000-1:2008, Section 7.9.2.2 - Text String Type:
    /// - If bytes start with 0xFE 0xFF, the string is UTF-16BE with BOM
    /// - Otherwise, it's PDFDocEncoding (superset of ISO Latin-1)
    fn decode_text_string(bytes: &[u8]) -> Option<String> {
        if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
            // UTF-16BE with BOM
            let utf16_bytes = &bytes[2..]; // Skip BOM

            // Convert bytes to u16 pairs (big-endian)
            let utf16_pairs: Vec<u16> = utf16_bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                .collect();

            String::from_utf16(&utf16_pairs).ok()
        } else {
            // PDFDocEncoding - use proper character mapping
            // ISO 32000-1:2008, Appendix D.2, Table D.2
            Some(
                bytes
                    .iter()
                    .filter_map(|&b| crate::fonts::font_dict::pdfdoc_encoding_lookup(b))
                    .collect(),
            )
        }
    }

    /// Extract all form fields from a PDF document.
    ///
    /// This function:
    /// 1. Gets the document catalog
    /// 2. Looks for /AcroForm dictionary
    /// 3. Extracts /Fields array
    /// 4. Recursively processes field hierarchy
    ///
    /// # Arguments
    ///
    /// * `doc` - The PDF document to extract fields from
    ///
    /// # Returns
    ///
    /// A vector of form fields, or an error if extraction fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use pdf_oxide::document::PdfDocument;
    /// use pdf_oxide::extractors::forms::FormExtractor;
    ///
    /// let mut doc = PdfDocument::open("form.pdf")?;
    /// let fields = FormExtractor::extract_fields(&mut doc)?;
    ///
    /// for field in &fields {
    ///     println!("Field: {} = {:?}", field.name, field.value);
    /// }
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn extract_fields(doc: &PdfDocument) -> Result<Vec<FormField>> {
        // Get document catalog
        let catalog = doc.catalog()?;
        let catalog_dict = catalog
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf("Catalog is not a dictionary".to_string()))?;

        // Check if AcroForm exists
        let acroform_ref = match catalog_dict.get("AcroForm") {
            Some(obj) => obj,
            None => {
                // No AcroForm in this document
                return Ok(Vec::new());
            }
        };

        // Resolve AcroForm dictionary
        let acroform = Self::resolve_object(doc, acroform_ref)?;
        let acroform_dict = acroform
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf("AcroForm is not a dictionary".to_string()))?;

        // Get Fields array
        let fields_ref = match acroform_dict.get("Fields") {
            Some(obj) => obj,
            None => {
                // AcroForm exists but has no fields
                return Ok(Vec::new());
            }
        };

        // Resolve fields array
        let fields_obj = Self::resolve_object(doc, fields_ref)?;
        let fields_array = fields_obj
            .as_array()
            .ok_or_else(|| Error::InvalidPdf("AcroForm /Fields is not an array".to_string()))?;

        // Extract fields recursively
        let mut result = Vec::new();
        for field_ref in fields_array {
            Self::extract_field_recursive(doc, field_ref, "", &mut result)?;
        }

        Ok(result)
    }

    /// Recursively extract a field and its children.
    ///
    /// PDF forms can have hierarchical field structure using /Kids arrays.
    /// This function handles:
    /// - Terminal fields (with /FT and /V)
    /// - Non-terminal fields (with /Kids but no /FT)
    /// - Inherited properties from parent fields
    ///
    /// # Arguments
    ///
    /// * `doc` - The PDF document
    /// * `field_ref` - Reference to the field object
    /// * `parent_name` - Full qualified name of parent field (for hierarchy)
    /// * `result` - Vector to accumulate extracted fields
    fn extract_field_recursive(
        doc: &PdfDocument,
        field_ref: &Object,
        parent_name: &str,
        result: &mut Vec<FormField>,
    ) -> Result<()> {
        // Capture object reference for later modification support
        let object_ref = field_ref.as_reference();

        // Resolve field dictionary
        let field = Self::resolve_object(doc, field_ref)?;
        let field_dict = match field.as_dict() {
            Some(d) => d,
            None => return Ok(()), // Skip if not a dictionary
        };

        // Get field name from /T (partial name)
        let partial_name = field_dict
            .get("T")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .and_then(|obj| obj.as_string().map(|s| s.to_vec()))
            .and_then(|bytes| Self::decode_text_string(&bytes))
            .unwrap_or_default();

        // Build full qualified name
        let full_name = if parent_name.is_empty() {
            partial_name.clone()
        } else if partial_name.is_empty() {
            parent_name.to_string()
        } else {
            format!("{}.{}", parent_name, partial_name)
        };

        // Recurse into Kids and also emit a row for parent fields
        // that carry a /T name, matching pypdf's traversal. The
        // previous behaviour recursed into Kids and dropped any
        // parent without an explicit /FT, which under-counted IRS
        // AcroForms by 15–30% (parent dictionaries for grouped
        // fields like multi-digit SSN boxes have /T but no /FT —
        // pypdf surfaces them, we did not).
        let has_kids = if let Some(kids_ref) = field_dict.get("Kids") {
            let kids = Self::resolve_object(doc, kids_ref)?;
            if let Some(kids_array) = kids.as_array() {
                for kid_ref in kids_array {
                    Self::extract_field_recursive(doc, kid_ref, &full_name, result)?;
                }
                true
            } else {
                false
            }
        } else {
            false
        };

        // Extract field type
        let field_type = field_dict
            .get("FT")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .and_then(|obj| obj.as_name().map(|s| s.to_string()))
            .map(|name| Self::parse_field_type(&name))
            .unwrap_or(FieldType::Unknown("".to_string()));

        // only skip the parent when it has NEITHER /FT
        // NOR /T. A parent with /T but no /FT (logical grouping like
        // `topmostSubform[0].Page1[0].FilingStatus[0]`) IS surfaced in
        // results, matching pypdf's behaviour. The terminal kids that
        // already got their own row carry the actual /V (checkbox
        // /Off/Yes state).
        let no_ft = matches!(field_type, FieldType::Unknown(ref s) if s.is_empty());
        if no_ft && (has_kids && partial_name.is_empty()) {
            // Truly anonymous parent with no /T — nothing to surface.
            return Ok(());
        }
        if no_ft && !has_kids && partial_name.is_empty() {
            // Leaf without /FT or /T — skip.
            return Ok(());
        }

        // Extract field value
        let value = field_dict
            .get("V")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .map(|obj| Self::parse_field_value(&obj, &field_type))
            .unwrap_or(FieldValue::None);

        // Extract tooltip
        let tooltip = field_dict
            .get("TU")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .and_then(|obj| obj.as_string().map(|s| s.to_vec()))
            .and_then(|bytes| Self::decode_text_string(&bytes));

        // Extract field bounds from /Rect [x1, y1, x2, y2]
        let bounds = field_dict
            .get("Rect")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .and_then(|obj| obj.as_array().cloned())
            .and_then(|arr| {
                if arr.len() == 4 {
                    let mut coords = Vec::with_capacity(4);
                    for item in &arr {
                        let val = match item {
                            Object::Integer(i) => Some(*i as f64),
                            Object::Real(f) => Some(*f),
                            _ => None,
                        }?;
                        coords.push(val);
                    }
                    Some([coords[0], coords[1], coords[2], coords[3]])
                } else {
                    None
                }
            });

        // Extract field flags /Ff
        let flags = field_dict
            .get("Ff")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .and_then(|obj| match obj {
                Object::Integer(i) => Some(i as u32),
                _ => None,
            });

        // Extract default value /DV
        let default_value = field_dict
            .get("DV")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .map(|obj| Self::parse_field_value(&obj, &field_type));

        // Extract max length /MaxLen (text fields only)
        let max_length = field_dict
            .get("MaxLen")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .and_then(|obj| match obj {
                Object::Integer(i) => Some(i as u32),
                _ => None,
            });

        // Extract text alignment /Q (0=left, 1=center, 2=right)
        let alignment = field_dict
            .get("Q")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .and_then(|obj| match obj {
                Object::Integer(i) => Some(i as u32),
                _ => None,
            });

        // Extract default appearance /DA
        let default_appearance = field_dict
            .get("DA")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .and_then(|obj| obj.as_string().map(|s| s.to_vec()))
            .and_then(|bytes| Self::decode_text_string(&bytes));

        // Extract border style /BS
        let border_style = field_dict
            .get("BS")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .and_then(|obj| Self::parse_border_style(&obj));

        // Extract appearance characteristics /MK
        let appearance_chars = field_dict
            .get("MK")
            .and_then(|obj| Self::resolve_object(doc, obj).ok())
            .and_then(|obj| Self::parse_appearance_characteristics(doc, &obj));

        // Create form field
        let form_field = FormField {
            name: partial_name,
            field_type,
            value,
            tooltip,
            full_name,
            bounds,
            object_ref,
            flags,
            default_value,
            max_length,
            alignment,
            default_appearance,
            border_style,
            appearance_chars,
        };

        result.push(form_field);
        Ok(())
    }

    /// Parse field type from /FT value.
    fn parse_field_type(ft: &str) -> FieldType {
        match ft {
            "Btn" => FieldType::Button,
            "Tx" => FieldType::Text,
            "Ch" => FieldType::Choice,
            "Sig" => FieldType::Signature,
            _ => FieldType::Unknown(ft.to_string()),
        }
    }

    /// Parse field value from /V object.
    fn parse_field_value(obj: &Object, field_type: &FieldType) -> FieldValue {
        match obj {
            Object::String(bytes) => {
                // Text string value - may be UTF-16BE or PDFDocEncoding
                if let Some(text) = Self::decode_text_string(bytes) {
                    FieldValue::Text(text)
                } else {
                    FieldValue::None
                }
            }
            Object::Name(name) => {
                // Name value (for radio buttons, choice fields)
                if *field_type == FieldType::Button {
                    // For checkboxes, common values are /Yes or /Off
                    if name == "Yes" || name == "On" {
                        FieldValue::Boolean(true)
                    } else if name == "No" || name == "Off" {
                        FieldValue::Boolean(false)
                    } else {
                        FieldValue::Name(name.clone())
                    }
                } else {
                    FieldValue::Name(name.clone())
                }
            }
            Object::Array(array) => {
                // Array of values (for multi-select list boxes)
                let values: Vec<String> = array
                    .iter()
                    .filter_map(|item| match item {
                        Object::String(bytes) => Self::decode_text_string(bytes),
                        Object::Name(name) => Some(name.clone()),
                        _ => None,
                    })
                    .collect();
                FieldValue::Array(values)
            }
            Object::Boolean(b) => {
                // Boolean value
                FieldValue::Boolean(*b)
            }
            _ => FieldValue::None,
        }
    }

    /// Parse border style from /BS dictionary.
    fn parse_border_style(obj: &Object) -> Option<BorderStyle> {
        let dict = obj.as_dict()?;

        let width = dict
            .get("W")
            .and_then(|o| match o {
                Object::Integer(i) => Some(*i as f32),
                Object::Real(f) => Some(*f as f32),
                _ => None,
            })
            .unwrap_or(1.0);

        let style = dict
            .get("S")
            .and_then(|o| o.as_name())
            .map(BorderStyleType::from_pdf_name)
            .unwrap_or(BorderStyleType::Solid);

        let dash_array = dict.get("D").and_then(|o| o.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|item| match item {
                    Object::Integer(i) => Some(*i as u32),
                    _ => None,
                })
                .collect()
        });

        Some(BorderStyle {
            width,
            style,
            dash_array,
        })
    }

    /// Parse appearance characteristics from /MK dictionary.
    fn parse_appearance_characteristics(
        doc: &PdfDocument,
        obj: &Object,
    ) -> Option<AppearanceCharacteristics> {
        let dict = obj.as_dict()?;

        let parse_color = |arr: &[Object]| -> Option<[f32; 3]> {
            if arr.len() == 3 {
                let r = match &arr[0] {
                    Object::Integer(i) => *i as f32,
                    Object::Real(f) => *f as f32,
                    _ => return None,
                };
                let g = match &arr[1] {
                    Object::Integer(i) => *i as f32,
                    Object::Real(f) => *f as f32,
                    _ => return None,
                };
                let b = match &arr[2] {
                    Object::Integer(i) => *i as f32,
                    Object::Real(f) => *f as f32,
                    _ => return None,
                };
                Some([r, g, b])
            } else {
                None
            }
        };

        let background_color = dict
            .get("BG")
            .and_then(|o| Self::resolve_object(doc, o).ok())
            .and_then(|o| o.as_array().cloned())
            .and_then(|arr| parse_color(&arr));

        let border_color = dict
            .get("BC")
            .and_then(|o| Self::resolve_object(doc, o).ok())
            .and_then(|o| o.as_array().cloned())
            .and_then(|arr| parse_color(&arr));

        let caption = dict
            .get("CA")
            .and_then(|o| Self::resolve_object(doc, o).ok())
            .and_then(|o| o.as_string().map(|s| s.to_vec()))
            .and_then(|bytes| Self::decode_text_string(&bytes));

        let rollover_caption = dict
            .get("RC")
            .and_then(|o| Self::resolve_object(doc, o).ok())
            .and_then(|o| o.as_string().map(|s| s.to_vec()))
            .and_then(|bytes| Self::decode_text_string(&bytes));

        let alternate_caption = dict
            .get("AC")
            .and_then(|o| Self::resolve_object(doc, o).ok())
            .and_then(|o| o.as_string().map(|s| s.to_vec()))
            .and_then(|bytes| Self::decode_text_string(&bytes));

        let rotation = dict.get("R").and_then(|o| match o {
            Object::Integer(i) => Some(*i as u32),
            _ => None,
        });

        Some(AppearanceCharacteristics {
            background_color,
            border_color,
            caption,
            rollover_caption,
            alternate_caption,
            rotation,
        })
    }
}

#[cfg(test)]
mod tests;
