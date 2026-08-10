#![warn(dead_code)]
#![warn(clippy::all)]

use super::*;

/// How usable this page's extracted text is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageTextStatus {
    /// Enough text of acceptable quality.
    Ready,
    /// Text exists, but its character quality or coverage is doubtful.
    Uncertain,
    /// No extractable text at all.
    Absent,
}

/// Whether this page's text can be used, and how much to trust it.
///
/// Answers only that question. It says nothing about whether the page has pictures, and
/// nothing about where the document came from.
#[derive(Clone, Copy, Debug)]
pub struct PageTextAssessment {
    /// Usability verdict.
    pub status: PageTextStatus,
    /// Characters recovered from the page.
    pub extracted_characters: usize,
    /// Share of recovered characters that are replacement, control, or private-use.
    pub invalid_character_ratio: f32,
}

/// What is visibly drawn on this page.
///
/// Derived from content operators and the XObject dictionary only. Nothing here
/// decompresses an image, rasterises, or runs OCR — doing any of that would put pixel
/// work on the cheap path that exists to avoid it.
#[derive(Clone, Copy, Debug)]
pub struct PageVisualAssessment {
    /// Fills, strokes, shadings, or inline images are present.
    pub has_paint_operations: bool,
    /// The page references at least one image XObject.
    pub has_image_xobjects: bool,
}

impl PageVisualAssessment {
    /// Whether anything would appear if the page were rendered.
    #[must_use]
    pub fn has_visible_content(&self) -> bool {
        self.has_paint_operations || self.has_image_xobjects
    }
}

impl PdfDocument {
    /// Judge the page's text without consulting anything visual.
    pub fn assess_page_text(&self, page: usize) -> Result<PageTextAssessment> {
        let spans = self.extract_spans(page)?;
        let mut characters = 0_usize;
        let mut invalid = 0_usize;
        for span in &spans {
            if span.artifact_type.is_some() {
                continue;
            }
            for character in span.text.chars() {
                characters += 1;
                if character == '\u{FFFD}'
                    || character.is_control()
                    || ('\u{E000}'..='\u{F8FF}').contains(&character)
                {
                    invalid += 1;
                }
            }
        }

        if characters == 0 {
            return Ok(PageTextAssessment {
                status: PageTextStatus::Absent,
                extracted_characters: 0,
                invalid_character_ratio: 0.0,
            });
        }

        let invalid_character_ratio = invalid as f32 / characters as f32;
        // The word-clustered form is what the quality gate is calibrated against; raw
        // span text makes every span boundary look like a word boundary.
        let word_text = self
            .extract_words(page)
            .unwrap_or_default()
            .into_iter()
            .map(|word| word.text)
            .collect::<Vec<_>>()
            .join(" ");
        let gated = crate::extractors::auto::text_quality_gate(&word_text).is_some();
        let status = if gated || invalid_character_ratio > 0.1 {
            PageTextStatus::Uncertain
        } else {
            PageTextStatus::Ready
        };

        Ok(PageTextAssessment {
            status,
            extracted_characters: characters,
            invalid_character_ratio,
        })
    }

    /// Judge what the page draws, from operators and resource dictionaries only.
    pub fn assess_page_visual(&self, page: usize) -> Result<PageVisualAssessment> {
        use crate::content::Operator;

        let content = self.get_page_content_data(page)?;
        let operators = crate::content::parse_content_stream(&content)?;

        let mut has_paint_operations = false;
        let mut painted_names: Vec<String> = Vec::new();
        for operator in &operators {
            match operator {
                Operator::Stroke
                | Operator::Fill
                | Operator::FillEvenOdd
                | Operator::FillStroke
                | Operator::FillStrokeEvenOdd
                | Operator::CloseFillStroke
                | Operator::CloseFillStrokeEvenOdd
                | Operator::PaintShading { .. }
                | Operator::InlineImage { .. } => has_paint_operations = true,
                Operator::Do { name } => painted_names.push(name.clone()),
                _ => {}
            }
        }

        let mut has_image_xobjects = false;
        if !painted_names.is_empty() {
            // Resolve the referenced names against /Resources /XObject and read only
            // each entry's dictionary. `extract_images()` would materialise the pixel
            // data, which is exactly what this assessment must not do.
            let resources = self.get_page_resources(page).ok();
            let xobjects = resources
                .as_ref()
                .and_then(|resources| resources.as_dict())
                .and_then(|dict| dict.get("XObject"))
                .map(|entry| self.resolve_obj_ref(entry));
            if let Some(dict) = xobjects.as_ref().and_then(|value| value.as_dict()) {
                for name in &painted_names {
                    let Some(entry) = dict.get(name) else {
                        continue;
                    };
                    let resolved = self.resolve_obj_ref(entry);
                    let subtype = resolved
                        .as_dict()
                        .or(match &resolved {
                            Object::Stream { dict, .. } => Some(dict),
                            _ => None,
                        })
                        .and_then(|entry_dict| entry_dict.get("Subtype"))
                        .and_then(Object::as_name);
                    match subtype {
                        Some("Image") => has_image_xobjects = true,
                        // A Form XObject paints whatever it contains; treating the
                        // invocation as visible content is the conservative reading and
                        // avoids recursing into another content stream here.
                        Some("Form") => has_paint_operations = true,
                        _ => {}
                    }
                }
            }
        }

        Ok(PageVisualAssessment {
            has_paint_operations,
            has_image_xobjects,
        })
    }
}
