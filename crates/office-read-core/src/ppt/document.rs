//! High-level PPT document API.

use std::io::{Read, Seek};

use crate::cfb::CfbReader;

use super::error::Result;
use super::text::{SlideText, TextType, extract_slides_text};

/// A parsed legacy PowerPoint document.
#[derive(Debug)]
pub struct PptDocument {
    /// Text content extracted from each slide.
    pub slides: Vec<SlideText>,
}

impl PptDocument {
    pub(crate) fn slide_markdown(&self, index: usize) -> Option<String> {
        let slide = self.slides.get(index)?;
        let mut out = String::new();
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&format!("## Slide {}\n\n", index + 1));
        for run in &slide.text_runs {
            if crate::budget::is_cancelled() {
                break;
            }
            match run.text_type {
                TextType::Title | TextType::CenterTitle => {
                    out.push_str("### ");
                    out.push_str(&run.text);
                    out.push_str("\n\n");
                }
                TextType::Notes => {
                    for line in run.text.lines() {
                        out.push_str("> ");
                        out.push_str(line);
                        out.push('\n');
                    }
                    out.push('\n');
                }
                _ => {
                    out.push_str(&run.text);
                    out.push_str("\n\n");
                }
            }
            if !crate::budget::markdown_within_limit(out.len()) {
                break;
            }
        }
        Some(out)
    }

    /// Open a PPT file from a reader.
    pub fn from_reader<R: Read + Seek>(reader: R) -> Result<Self> {
        let mut cfb = CfbReader::new(reader)?;

        let stream = match cfb
            .open_stream("PowerPoint Document")
            .or_else(|_| cfb.open_stream("PP97_DUALSTORAGE"))
        {
            Ok(s) => s,
            Err(_) => {
                return Err(super::error::PptError::MissingStream(
                    "PowerPoint Document".into(),
                ));
            }
        };

        let current_user = cfb.open_stream("Current User").ok();
        let slides = extract_slides_text(&stream, current_user.as_deref())?;

        if slides.is_empty() {
            return Err(super::error::PptError::Corrupted(
                "no readable slide structure".into(),
            ));
        }
        Ok(Self { slides })
    }

    /// Convert to markdown.
    #[cfg(test)]
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        for (i, slide) in self.slides.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&format!("## Slide {}\n\n", i + 1));

            for run in &slide.text_runs {
                match run.text_type {
                    TextType::Title | TextType::CenterTitle => {
                        out.push_str("### ");
                        out.push_str(&run.text);
                        out.push_str("\n\n");
                    }
                    TextType::Notes => {
                        for line in run.text.lines() {
                            out.push_str("> ");
                            out.push_str(line);
                            out.push('\n');
                        }
                        out.push('\n');
                    }
                    _ => {
                        out.push_str(&run.text);
                        out.push_str("\n\n");
                    }
                }
            }
        }
        out
    }
}
