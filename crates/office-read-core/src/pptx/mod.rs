//! # office_oxide::pptx
//!
//! High-performance PowerPoint presentation (.pptx) processing.
//!
//! Read, convert, and extract content from PPTX files
//! (Office Open XML PresentationML, ISO 29500 / ECMA-376).
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use office_oxide::pptx::PptxDocument;
//!
//! let doc = PptxDocument::open("slides.pptx").unwrap();
//! println!("{}", doc.plain_text());
//! println!("{}", doc.to_markdown());
//! ```

/// In-place editing of PPTX documents.
/// Error types for PPTX parsing and creation.
pub mod error;
/// `ppt/presentation.xml` data model.
pub mod presentation;
/// Shape data model for PresentationML slides.
pub mod shape;
/// Slide XML parser.
pub mod slide;
/// Text extraction utilities for PPTX.
pub mod text;
/// PPTX creation (write) API.
pub use error::{PptxError, Result};
pub use presentation::PresentationInfo;
pub use slide::Slide;

use std::io::{Read, Seek};

use crate::core::opc::OpcReader;
use crate::core::relationships::{Relationships, TargetMode, rel_types};
use log::debug;

/// A parsed PPTX document.
#[derive(Debug, Clone)]
pub struct PptxDocument {
    /// Parsed slides, in presentation order.
    pub slides: Vec<Slide>,
}

impl PptxDocument {
    /// Open a PPTX document from any `Read + Seek` source.
    pub fn from_reader<R: Read + Seek>(reader: R) -> Result<Self> {
        let opc = OpcReader::new(reader)?;
        Self::from_opc(opc)
    }

    fn from_opc<R: Read + Seek>(mut opc: OpcReader<R>) -> Result<Self> {
        debug!("PptxDocument: parsing started");
        let main_part = opc.main_document_part()?;
        let pres_rels = opc.read_rels_for(&main_part)?;

        // Parse presentation.xml
        let pres_data = opc.read_part(&main_part)?;
        let presentation = PresentationInfo::parse(&pres_data)?;

        // Phase 1: gather raw data sequentially (requires &mut opc)
        struct SlideBundle {
            slide_data: Vec<u8>,
            slide_rels: Relationships,
            notes_data: Option<Vec<u8>>,
        }
        crate::budget::check_model_items("office_pptx_slides", presentation.slides.len())
            .map_err(crate::map_office_error_to_core)?;
        let mut bundles = Vec::with_capacity(presentation.slides.len());
        for slide_id in &presentation.slides {
            let part_name = pres_rels.resolve_target(&slide_id.rel_id, &main_part)?;
            let slide_rels = opc.read_rels_for(&part_name)?;
            let slide_data = opc.read_part(&part_name)?;

            let notes_data =
                if let Some(notes_rel) = slide_rels.first_by_type(rel_types::NOTES_SLIDE) {
                    if notes_rel.target_mode != TargetMode::Internal {
                        return Err(crate::core::Error::MalformedXml(
                            "notes relationship must be internal".to_owned(),
                        )
                        .into());
                    }
                    let notes_part = part_name.resolve_relative(&notes_rel.target)?;
                    Some(opc.read_part(&notes_part)?)
                } else {
                    None
                };

            bundles.push(SlideBundle {
                slide_data,
                slide_rels,
                notes_data,
            });
        }

        let slides = bundles
            .into_iter()
            .map(|bundle| {
                let mut parsed = Slide::parse(&bundle.slide_data, &bundle.slide_rels)?;
                if let Some(notes_data) = &bundle.notes_data {
                    parsed.notes = extract_notes_text(notes_data)?;
                }
                Ok(parsed)
            })
            .collect::<Result<Vec<_>>>()?;

        debug!("PptxDocument: {} slides parsed", slides.len());
        Ok(PptxDocument { slides })
    }
}

/// Extract speaker notes plain text from a notes slide XML.
/// Finds the body placeholder (type="body") and extracts its text.
fn extract_notes_text(xml_data: &[u8]) -> crate::core::Result<Option<String>> {
    slide::extract_notes_text(xml_data)
}
