//! # office_oxide::docx
//!
//! High-performance Word document (.docx) processing.
//!
//! Read, convert, and extract content from DOCX files
//! (Office Open XML WordprocessingML, ISO 29500 / ECMA-376).
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use office_oxide::docx::DocxDocument;
//!
//! let doc = DocxDocument::open("report.docx").unwrap();
//! println!("{}", doc.plain_text());
//! println!("{}", doc.to_markdown());
//! ```

/// Document body and block-level element types.
pub mod document;
/// In-place editing of existing DOCX files.
/// DOCX-specific error type.
pub mod error;
/// Run and paragraph formatting types (`RunProperties`, `ParagraphProperties`, etc.).
pub mod formatting;
/// Section properties, headers, footers, page size/margin types.
pub mod headers;
/// Hyperlink types (`Hyperlink`, `HyperlinkTarget`).
pub mod hyperlink;
/// Drawing/image reference type (`DrawingInfo`).
pub mod image;
/// Numbering definitions and list format types.
pub mod numbering;
/// Paragraph, run, and inline content types.
pub mod paragraph;
/// Style sheet and style definition types.
pub mod styles;
/// Table structure types.
pub mod table;
/// Text extraction and markdown rendering for DOCX.
pub mod text;
/// DOCX creation (write) API.
pub use document::{BlockElement, Body};
pub use error::{DocxError, Result};
pub use headers::{HeaderFooter, SectionProperties};
pub use hyperlink::{Hyperlink, HyperlinkTarget};
pub use image::DrawingInfo;
pub use numbering::NumberingDefinitions;
pub use paragraph::{BreakType, Paragraph, ParagraphContent, Run, RunContent};
pub use styles::StyleSheet;
pub use table::{Table, TableCell, TableRow};

use std::io::{Read, Seek};

use log::debug;
use quick_xml::events::Event;

use crate::core::opc::OpcReader;
use crate::core::relationships::{TargetMode, rel_types};
use crate::core::xml;

use self::formatting::{parse_paragraph_properties_fast, parse_run_properties_fast};
use self::headers::HeaderFooterRef;

// Use crate::core::Result internally for all XML parsing (it has From<quick_xml::Error>).
// DocxError wraps crate::core::Error, so conversion at the public boundary is automatic via `?`.
type CoreResult<T> = crate::core::Result<T>;

/// Create a fast reader that does NOT trim text content.
/// Unlike `xml::make_reader`, this preserves whitespace so `xml:space="preserve"` works.
fn make_content_reader(xml_data: &[u8]) -> quick_xml::Reader<&[u8]> {
    let mut reader = quick_xml::Reader::from_reader(xml_data);
    reader.config_mut().check_end_names = false;
    reader.config_mut().check_comments = false;
    reader
}

fn charge_model_item(resource: &'static str) -> CoreResult<()> {
    crate::budget::charge_model_items(resource, 1).map_err(crate::map_office_error_to_core)
}

fn charge_model_text(resource: &'static str, bytes: usize) -> CoreResult<()> {
    crate::budget::charge_model_text(resource, bytes).map_err(crate::map_office_error_to_core)
}

/// A parsed DOCX document.
#[derive(Debug, Clone)]
pub struct DocxDocument {
    /// The document body.
    pub body: Body,
    /// Parsed stylesheet from `word/styles.xml`.
    pub styles: Option<StyleSheet>,
    /// Numbering definitions from `word/numbering.xml`.
    pub numbering: Option<NumberingDefinitions>,
    /// Parsed headers and footers.
    pub headers_footers: Vec<HeaderFooter>,
}

impl DocxDocument {
    /// Open a DOCX document from any `Read + Seek` source.
    pub fn from_reader<R: Read + Seek>(reader: R) -> Result<Self> {
        let opc = OpcReader::new(reader)?;
        Self::from_opc(opc)
    }

    fn from_opc<R: Read + Seek>(mut opc: OpcReader<R>) -> Result<Self> {
        debug!("DocxDocument: parsing started");
        let main_part = opc.main_document_part()?;
        let doc_rels = opc.read_rels_for(&main_part)?;

        // Parse styles
        let styles = if let Some(rel) = doc_rels.first_by_type(rel_types::STYLES) {
            if rel.target_mode != TargetMode::Internal {
                return Err(crate::core::Error::MalformedXml(
                    "styles relationship must be internal".to_owned(),
                )
                .into());
            }
            let part_name = main_part.resolve_relative(&rel.target)?;
            let data = opc.read_part(&part_name)?;
            Some(StyleSheet::parse(&data)?)
        } else {
            None
        };

        // Numbering is optional, but a declared relationship must resolve.
        let numbering = if let Some(rel) = doc_rels.first_by_type(rel_types::NUMBERING) {
            if rel.target_mode != TargetMode::Internal {
                return Err(crate::core::Error::MalformedXml(
                    "numbering relationship must be internal".to_owned(),
                )
                .into());
            }
            let part_name = main_part.resolve_relative(&rel.target)?;
            let data = opc.read_part(&part_name)?;
            Some(NumberingDefinitions::parse(&data)?)
        } else {
            None
        };

        // Parse main document
        let doc_data = opc.read_part(&main_part)?;
        let (body, sections) = parse_document(&doc_data, &doc_rels)?;

        // Parse headers and footers. Walk header refs and footer refs
        // separately so each parsed `HeaderFooter` can record its own
        // role; without that distinction, downstream consumers had to
        // back-derive headers-vs-footers from cumulative ref counts,
        // which silently misclassifies entries in multi-section docs.
        let mut headers_footers = Vec::new();
        let mut parse_hf = |hf_ref: &HeaderFooterRef, is_header: bool| -> CoreResult<()> {
            let rel = doc_rels.get_by_id(&hf_ref.relationship_id).ok_or_else(|| {
                crate::core::Error::RelationshipNotFound(hf_ref.relationship_id.clone())
            })?;
            if rel.target_mode != TargetMode::Internal {
                return Err(crate::core::Error::MalformedXml(
                    "header/footer relationship must be internal".to_owned(),
                ));
            }
            let part_name = main_part.resolve_relative(&rel.target)?;
            let data = opc.read_part(&part_name)?;
            let content = parse_body_elements(&data)?;
            headers_footers.push(HeaderFooter { content, is_header });
            Ok(())
        };
        for section in &sections {
            for hf_ref in &section.header_refs {
                parse_hf(hf_ref, true)?;
            }
            for hf_ref in &section.footer_refs {
                parse_hf(hf_ref, false)?;
            }
        }

        debug!(
            "DocxDocument: {} block elements, {} sections",
            body.elements.len(),
            sections.len()
        );
        Ok(DocxDocument {
            body,
            styles,
            numbering,
            headers_footers,
        })
    }
}

/// Parse body-level elements from XML (used for headers/footers which share the same structure).
fn parse_body_elements(xml_data: &[u8]) -> CoreResult<Vec<BlockElement>> {
    let mut reader = make_content_reader(xml_data);
    let mut elements = Vec::new();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"p" => {
                    charge_model_item("office_docx_model_items")?;
                    elements.push(BlockElement::Paragraph(parse_paragraph(&mut reader)?));
                }
                b"tbl" => {
                    charge_model_item("office_docx_model_items")?;
                    elements.push(BlockElement::Table(parse_table(&mut reader)?));
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(elements)
}

/// Parse `word/document.xml` and return the Body and SectionProperties.
fn parse_document(
    xml_data: &[u8],
    rels: &crate::core::relationships::Relationships,
) -> CoreResult<(Body, Vec<SectionProperties>)> {
    let mut reader = make_content_reader(xml_data);
    let mut elements = Vec::new();
    let mut sections = Vec::new();
    let mut in_body = false;

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"body" => {
                    in_body = true;
                }
                b"p" if in_body => {
                    charge_model_item("office_docx_model_items")?;
                    elements.push(BlockElement::Paragraph(parse_paragraph(&mut reader)?));
                }
                b"tbl" if in_body => {
                    charge_model_item("office_docx_model_items")?;
                    elements.push(BlockElement::Table(parse_table(&mut reader)?));
                }
                b"sectPr" if in_body => {
                    charge_model_item("office_docx_model_items")?;
                    sections.push(parse_section_properties(&mut reader, e)?);
                }
                _ => {}
            },
            Event::End(ref e) if e.local_name().as_ref() == b"body" => {
                in_body = false;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    // Resolve hyperlink targets using relationships
    resolve_hyperlinks(&mut elements, rels)?;

    // Detect mid-document section breaks: paragraphs whose <w:pPr>
    // carries a <w:sectPr>. Each such paragraph terminates a section,
    // and its sectPr describes the section that ends there. Trailing
    // elements after the last break belong to a final section
    // described by the body-level sectPr (already in `sections`).
    let mut break_sections: Vec<SectionProperties> = Vec::new();
    for el in &elements {
        if let BlockElement::Paragraph(p) = el {
            if let Some(props) = &p.properties {
                if let Some(sp) = &props.section_properties {
                    break_sections.push(sp.clone());
                }
            }
        }
    }
    // Stitch break-derived section_properties in front of the
    // body-level final sectPr so the section list is in document order.
    let mut all_sections = break_sections;
    all_sections.extend(sections);

    let body = Body { elements };
    Ok((body, all_sections))
}

/// Walk the element tree and resolve hyperlink rIds to actual URLs.
fn resolve_hyperlinks(
    elements: &mut [BlockElement],
    rels: &crate::core::relationships::Relationships,
) -> CoreResult<()> {
    for elem in elements.iter_mut() {
        match elem {
            BlockElement::Paragraph(p) => {
                for content in &mut p.content {
                    if let ParagraphContent::Hyperlink(hl) = content {
                        if let HyperlinkTarget::External(ref r_id) = hl.target {
                            let rel = rels.get_by_id(r_id).ok_or_else(|| {
                                crate::core::Error::RelationshipNotFound(r_id.clone())
                            })?;
                            charge_model_text("office_docx_text_bytes", rel.target.len())?;
                            if rel.target_mode == TargetMode::External {
                                hl.target = HyperlinkTarget::External(rel.target.clone());
                            } else {
                                hl.target = HyperlinkTarget::Internal(rel.target.clone());
                            }
                        }
                    }
                }
            }
            BlockElement::Table(t) => {
                for row in &mut t.rows {
                    for cell in &mut row.cells {
                        resolve_hyperlinks(&mut cell.content, rels)?;
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Paragraph parsing
// ---------------------------------------------------------------------------

fn parse_paragraph(reader: &mut quick_xml::Reader<&[u8]>) -> CoreResult<Paragraph> {
    let mut paragraph = Paragraph::default();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"pPr" => {
                    paragraph.properties = Some(parse_paragraph_properties_fast(reader)?);
                }
                b"r" => {
                    charge_model_item("office_docx_model_items")?;
                    paragraph
                        .content
                        .push(ParagraphContent::Run(parse_run(reader)?));
                }
                b"hyperlink" => {
                    charge_model_item("office_docx_model_items")?;
                    paragraph
                        .content
                        .push(ParagraphContent::Hyperlink(parse_hyperlink(reader, e)?));
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::End(ref e) if e.local_name().as_ref() == b"p" => {
                break;
            }
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated paragraph".to_owned(),
                ));
            }
            _ => {}
        }
    }
    Ok(paragraph)
}

fn parse_run(reader: &mut quick_xml::Reader<&[u8]>) -> CoreResult<Run> {
    let mut run = Run::default();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"rPr" => {
                    run.properties = Some(parse_run_properties_fast(reader)?);
                }
                b"t" => {
                    let text = xml::read_text_content_fast(reader)?;
                    if !text.is_empty() {
                        charge_model_item("office_docx_model_items")?;
                        charge_model_text("office_docx_text_bytes", text.len())?;
                        run.content.push(RunContent::Text(text));
                    }
                }
                b"br" => {
                    let break_type = match xml::optional_attr_str(e, b"w:type")? {
                        Some(ref t) => match t.as_ref() {
                            "page" => BreakType::Page,
                            "column" => BreakType::Column,
                            _ => BreakType::Line,
                        },
                        None => BreakType::Line,
                    };
                    charge_model_item("office_docx_model_items")?;
                    run.content.push(RunContent::Break(break_type));
                    xml::skip_element_fast(reader)?;
                }
                b"drawing" => {
                    if let Some(drawing) = parse_drawing(reader)? {
                        charge_model_item("office_docx_model_items")?;
                        run.content.push(RunContent::Drawing(drawing));
                    }
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::Empty(ref e) => match e.local_name().as_ref() {
                b"br" => {
                    let break_type = match xml::optional_attr_str(e, b"w:type")? {
                        Some(ref t) => match t.as_ref() {
                            "page" => BreakType::Page,
                            "column" => BreakType::Column,
                            _ => BreakType::Line,
                        },
                        None => BreakType::Line,
                    };
                    charge_model_item("office_docx_model_items")?;
                    run.content.push(RunContent::Break(break_type));
                }
                b"tab" => {
                    charge_model_item("office_docx_model_items")?;
                    run.content.push(RunContent::Tab);
                }
                _ => {}
            },
            Event::End(ref e) if e.local_name().as_ref() == b"r" => {
                break;
            }
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml("truncated run".to_owned()));
            }
            _ => {}
        }
    }
    Ok(run)
}

fn parse_hyperlink(
    reader: &mut quick_xml::Reader<&[u8]>,
    start: &quick_xml::events::BytesStart,
) -> CoreResult<Hyperlink> {
    // Determine target: r:id for external, w:anchor for internal
    let r_id = xml::optional_attr_str(start, b"r:id")?.map(|v| v.into_owned());
    let anchor = xml::optional_attr_str(start, b"w:anchor")?.map(|v| v.into_owned());

    let target = if let Some(anchor) = anchor {
        charge_model_text("office_docx_text_bytes", anchor.len())?;
        HyperlinkTarget::Internal(anchor)
    } else if let Some(r_id) = r_id {
        charge_model_text("office_docx_text_bytes", r_id.len())?;
        // Will be resolved to actual URL after parsing via resolve_hyperlinks()
        HyperlinkTarget::External(r_id)
    } else {
        return Err(crate::core::Error::MalformedXml(
            "hyperlink has no target".to_owned(),
        ));
    };

    let mut runs = Vec::new();
    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) => {
                if e.local_name().as_ref() == b"r" {
                    charge_model_item("office_docx_model_items")?;
                    runs.push(parse_run(reader)?);
                } else {
                    xml::skip_element_fast(reader)?;
                }
            }
            Event::End(ref e) if e.local_name().as_ref() == b"hyperlink" => {
                break;
            }
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated hyperlink".to_owned(),
                ));
            }
            _ => {}
        }
    }

    Ok(Hyperlink { target, runs })
}

// ---------------------------------------------------------------------------
// Drawing / image parsing
// ---------------------------------------------------------------------------

/// Parse a `<w:drawing>` element. The opening tag has already been
/// consumed by the caller, so we drive forward until the matching
/// `</w:drawing>` End event.
///
/// A drawing wraps either `<wp:inline>` or `<wp:anchor>` (anchor =
/// floating). Everything we care about lives inside that single
/// wrapper, so we delegate to `parse_inline_or_anchor_body` and treat
/// any other top-level event as ignorable filler.
fn parse_drawing(reader: &mut quick_xml::Reader<&[u8]>) -> CoreResult<Option<DrawingInfo>> {
    let mut info: Option<DrawingInfo> = None;

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"inline" => {
                    info = parse_inline_or_anchor_body(reader, /*inline=*/ true, b"inline")?;
                }
                b"anchor" => {
                    info = parse_inline_or_anchor_body(reader, /*inline=*/ false, b"anchor")?;
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::End(ref e) if e.local_name().as_ref() == b"drawing" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated drawing".to_owned(),
                ));
            }
            _ => {}
        }
    }

    Ok(info)
}

/// Parse only the image relationship and accessibility description from an
/// inline or anchored drawing. Geometry and raw media are intentionally ignored.
fn parse_inline_or_anchor_body(
    reader: &mut quick_xml::Reader<&[u8]>,
    _inline: bool,
    end_local: &[u8],
) -> CoreResult<Option<DrawingInfo>> {
    let mut description = None;
    let mut relationship_id = None;

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) if e.local_name().as_ref() == b"docPr" => {
                description = xml::optional_attr_str(e, b"descr")?.map(|value| value.into_owned());
                xml::skip_element_fast(reader)?;
            }
            Event::Empty(ref e) if e.local_name().as_ref() == b"docPr" => {
                description = xml::optional_attr_str(e, b"descr")?.map(|value| value.into_owned());
            }
            Event::Start(ref e) if e.local_name().as_ref() == b"blip" => {
                relationship_id =
                    xml::optional_prefixed_attr_str(e, b"embed")?.map(|value| value.into_owned());
                xml::skip_element_fast(reader)?;
            }
            Event::Empty(ref e) if e.local_name().as_ref() == b"blip" => {
                relationship_id =
                    xml::optional_prefixed_attr_str(e, b"embed")?.map(|value| value.into_owned());
            }
            Event::Start(_) => {}
            Event::End(ref e) if e.local_name().as_ref() == end_local => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated drawing".to_string(),
                ));
            }
            _ => {}
        }
    }

    if relationship_id.is_some() || description.is_some() {
        let text_bytes = relationship_id.as_ref().map_or(0, String::len)
            + description.as_ref().map_or(0, String::len);
        charge_model_text("office_docx_text_bytes", text_bytes)?;
        Ok(Some(DrawingInfo {
            relationship_id: relationship_id.unwrap_or_default(),
            description,
        }))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Table parsing
// ---------------------------------------------------------------------------

fn parse_table(reader: &mut quick_xml::Reader<&[u8]>) -> CoreResult<Table> {
    let mut rows = Vec::new();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) if e.local_name().as_ref() == b"tr" => {
                charge_model_item("office_docx_model_items")?;
                rows.push(parse_table_row(reader)?);
            }
            Event::Start(_) => {}
            Event::End(ref e) if e.local_name().as_ref() == b"tbl" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated table".to_string(),
                ));
            }
            _ => {}
        }
    }

    Ok(Table { rows })
}

fn parse_table_row(reader: &mut quick_xml::Reader<&[u8]>) -> CoreResult<TableRow> {
    let mut cells = Vec::new();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) if e.local_name().as_ref() == b"tc" => {
                charge_model_item("office_docx_model_items")?;
                cells.push(parse_table_cell(reader)?);
            }
            Event::Start(_) => {}
            Event::End(ref e) if e.local_name().as_ref() == b"tr" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated table row".to_string(),
                ));
            }
            _ => {}
        }
    }

    Ok(TableRow { cells })
}

fn parse_table_cell(reader: &mut quick_xml::Reader<&[u8]>) -> CoreResult<TableCell> {
    let mut content = Vec::new();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) if e.local_name().as_ref() == b"p" => {
                charge_model_item("office_docx_model_items")?;
                content.push(BlockElement::Paragraph(parse_paragraph(reader)?));
            }
            Event::Start(ref e) if e.local_name().as_ref() == b"tbl" => {
                charge_model_item("office_docx_model_items")?;
                content.push(BlockElement::Table(parse_table(reader)?));
            }
            Event::Start(_) => {}
            Event::End(ref e) if e.local_name().as_ref() == b"tc" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated table cell".to_string(),
                ));
            }
            _ => {}
        }
    }

    Ok(TableCell { content })
}

// ---------------------------------------------------------------------------
// Section properties parsing
// ---------------------------------------------------------------------------

pub(crate) fn parse_section_properties(
    reader: &mut quick_xml::Reader<&[u8]>,
    _start: &quick_xml::events::BytesStart,
) -> CoreResult<SectionProperties> {
    let mut props = SectionProperties::default();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) | Event::Empty(ref e) => match e.local_name().as_ref() {
                b"headerReference" => {
                    let rid = xml::optional_prefixed_attr_str(e, b"id")?.ok_or_else(|| {
                        crate::core::Error::MissingAttribute {
                            element: "headerReference".to_owned(),
                            attr: "r:id".to_owned(),
                        }
                    })?;
                    charge_model_item("office_docx_model_items")?;
                    charge_model_text("office_docx_text_bytes", rid.len())?;
                    props.header_refs.push(HeaderFooterRef {
                        relationship_id: rid.into_owned(),
                    });
                }
                b"footerReference" => {
                    let rid = xml::optional_prefixed_attr_str(e, b"id")?.ok_or_else(|| {
                        crate::core::Error::MissingAttribute {
                            element: "footerReference".to_owned(),
                            attr: "r:id".to_owned(),
                        }
                    })?;
                    charge_model_item("office_docx_model_items")?;
                    charge_model_text("office_docx_text_bytes", rid.len())?;
                    props.footer_refs.push(HeaderFooterRef {
                        relationship_id: rid.into_owned(),
                    });
                }
                _ => {}
            },
            Event::End(ref e) if e.local_name().as_ref() == b"sectPr" => {
                break;
            }
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated section properties".to_owned(),
                ));
            }
            _ => {}
        }
    }
    Ok(props)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(any())]
mod tests {
    use super::*;
    use std::io::Cursor;

    use crate::core::opc::{OpcWriter, PartName};

    fn make_minimal_docx(document_xml: &[u8]) -> Vec<u8> {
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = OpcWriter::new(cursor).unwrap();

        let doc_part = PartName::new("/word/document.xml").unwrap();
        writer
            .add_part(
                &doc_part,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
                document_xml,
            )
            .unwrap();
        writer.add_package_rel(rel_types::OFFICE_DOCUMENT, "word/document.xml");

        let result = writer.finish().unwrap();
        result.into_inner()
    }

    #[test]
    fn parse_empty_document() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body/>
</w:document>"#;
        let data = make_minimal_docx(xml);
        let doc = DocxDocument::from_reader(Cursor::new(data)).unwrap();
        assert!(doc.body.elements.is_empty());
        assert_eq!(doc.plain_text(), "");
    }

    #[test]
    fn parse_single_paragraph() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:r>
        <w:t>Hello, World!</w:t>
      </w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let data = make_minimal_docx(xml);
        let doc = DocxDocument::from_reader(Cursor::new(data)).unwrap();
        assert_eq!(doc.body.elements.len(), 1);
        assert_eq!(doc.plain_text(), "Hello, World!");
    }

    #[test]
    fn parse_multiple_paragraphs() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:r><w:t>First paragraph.</w:t></w:r>
    </w:p>
    <w:p>
      <w:r><w:t>Second paragraph.</w:t></w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let data = make_minimal_docx(xml);
        let doc = DocxDocument::from_reader(Cursor::new(data)).unwrap();
        assert_eq!(doc.body.elements.len(), 2);
        assert_eq!(doc.plain_text(), "First paragraph.\nSecond paragraph.");
    }

    #[test]
    fn parse_multiple_runs() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:r><w:t xml:space="preserve">Hello </w:t></w:r>
      <w:r><w:t>World</w:t></w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let data = make_minimal_docx(xml);
        let doc = DocxDocument::from_reader(Cursor::new(data)).unwrap();
        assert_eq!(doc.plain_text(), "Hello World");
    }

    #[test]
    fn parse_break_and_tab() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:r>
        <w:t>Before</w:t>
        <w:tab/>
        <w:t>After</w:t>
      </w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let data = make_minimal_docx(xml);
        let doc = DocxDocument::from_reader(Cursor::new(data)).unwrap();
        assert_eq!(doc.plain_text(), "Before\tAfter");
    }

    #[test]
    fn parse_table_basic() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:tbl>
      <w:tr>
        <w:tc><w:p><w:r><w:t>A1</w:t></w:r></w:p></w:tc>
        <w:tc><w:p><w:r><w:t>B1</w:t></w:r></w:p></w:tc>
      </w:tr>
      <w:tr>
        <w:tc><w:p><w:r><w:t>A2</w:t></w:r></w:p></w:tc>
        <w:tc><w:p><w:r><w:t>B2</w:t></w:r></w:p></w:tc>
      </w:tr>
    </w:tbl>
  </w:body>
</w:document>"#;
        let data = make_minimal_docx(xml);
        let doc = DocxDocument::from_reader(Cursor::new(data)).unwrap();
        assert_eq!(doc.body.elements.len(), 1);
        if let BlockElement::Table(ref table) = doc.body.elements[0] {
            assert_eq!(table.rows.len(), 2);
            assert_eq!(table.rows[0].cells.len(), 2);
        } else {
            panic!("expected table");
        }
        assert_eq!(doc.plain_text(), "A1\tB1\nA2\tB2");
    }

    #[test]
    fn parse_paragraph_with_formatting() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:pPr>
        <w:pStyle w:val="Heading1"/>
        <w:jc w:val="center"/>
      </w:pPr>
      <w:r>
        <w:rPr>
          <w:b/>
          <w:sz w:val="32"/>
        </w:rPr>
        <w:t>Bold Heading</w:t>
      </w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let data = make_minimal_docx(xml);
        let doc = DocxDocument::from_reader(Cursor::new(data)).unwrap();

        if let BlockElement::Paragraph(ref p) = doc.body.elements[0] {
            let pp = p.properties.as_ref().unwrap();
            assert_eq!(pp.style_id.as_deref(), Some("Heading1"));
            assert_eq!(pp.justification, Some(Justification::Center));

            if let ParagraphContent::Run(ref run) = p.content[0] {
                let rp = run.properties.as_ref().unwrap();
                assert_eq!(rp.bold, Some(true));
                assert_eq!(rp.font_size, Some(crate::core::units::HalfPoint(32)));
            } else {
                panic!("expected run");
            }
        } else {
            panic!("expected paragraph");
        }
    }

    #[test]
    fn markdown_bold_italic() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:r>
        <w:rPr><w:b/></w:rPr>
        <w:t>bold</w:t>
      </w:r>
      <w:r>
        <w:t xml:space="preserve"> and </w:t>
      </w:r>
      <w:r>
        <w:rPr><w:i/></w:rPr>
        <w:t>italic</w:t>
      </w:r>
    </w:p>
  </w:body>
</w:document>"#;
        let data = make_minimal_docx(xml);
        let doc = DocxDocument::from_reader(Cursor::new(data)).unwrap();
        assert_eq!(doc.to_markdown(), "**bold** and *italic*");
    }

    #[test]
    fn markdown_table() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:tbl>
      <w:tr>
        <w:tc><w:p><w:r><w:t>Header1</w:t></w:r></w:p></w:tc>
        <w:tc><w:p><w:r><w:t>Header2</w:t></w:r></w:p></w:tc>
      </w:tr>
      <w:tr>
        <w:tc><w:p><w:r><w:t>Cell1</w:t></w:r></w:p></w:tc>
        <w:tc><w:p><w:r><w:t>Cell2</w:t></w:r></w:p></w:tc>
      </w:tr>
    </w:tbl>
  </w:body>
</w:document>"#;
        let data = make_minimal_docx(xml);
        let doc = DocxDocument::from_reader(Cursor::new(data)).unwrap();
        let md = doc.to_markdown();
        assert!(md.contains("| Header1 | Header2 |"));
        assert!(md.contains("| --- | --- |"));
        assert!(md.contains("| Cell1 | Cell2 |"));
    }

    #[test]
    fn parse_drawing_anchor_position() {
        let xml =
            br#"<w:drawing xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
                xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
                xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
            <wp:anchor>
                <wp:positionH relativeFrom="page"><wp:posOffset>914400</wp:posOffset></wp:positionH>
                <wp:positionV relativeFrom="page"><wp:posOffset>457200</wp:posOffset></wp:positionV>
                <wp:extent cx="2000000" cy="1500000"/>
                <a:graphic><a:graphicData uri="">
                    <pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">
                        <pic:blipFill><a:blip r:embed="rId7"/></pic:blipFill>
                    </pic:pic>
                </a:graphicData></a:graphic>
            </wp:anchor>
        </w:drawing>"#;
        let mut reader = make_content_reader(xml);
        // Advance past the outer <w:drawing> Start so parse_drawing
        // sees the inner contents (it expects to be entered with
        // depth=1 already accounting for that wrapper).
        loop {
            match reader.read_event().unwrap() {
                quick_xml::events::Event::Start(ref e) if e.local_name().as_ref() == b"drawing" => {
                    break;
                }
                quick_xml::events::Event::Eof => panic!("no drawing"),
                _ => {}
            }
        }
        let info = parse_drawing(&mut reader).unwrap().expect("drawing");
        assert!(!info.inline);
        let pos = info.anchor_position.expect("anchor position");
        assert_eq!(pos.x_emu, 914400);
        assert_eq!(pos.y_emu, 457200);
        assert_eq!(pos.h_relative_from, crate::docx::AnchorFrame::Page);
        assert_eq!(info.relationship_id, "rId7");
    }

    #[test]
    fn parse_drawing_wsp_line_shape() {
        let xml =
            br#"<w:drawing xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
                xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
                xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
            <wp:anchor>
                <wp:positionH relativeFrom="page"><wp:posOffset>100000</wp:posOffset></wp:positionH>
                <wp:positionV relativeFrom="page"><wp:posOffset>200000</wp:posOffset></wp:positionV>
                <wp:extent cx="500000" cy="0"/>
                <a:graphic><a:graphicData>
                    <wps:wsp>
                        <wps:spPr>
                            <a:prstGeom prst="line"/>
                            <a:ln w="9525">
                                <a:solidFill><a:srgbClr val="FF0000"/></a:solidFill>
                            </a:ln>
                        </wps:spPr>
                    </wps:wsp>
                </a:graphicData></a:graphic>
            </wp:anchor>
        </w:drawing>"#;
        let mut reader = make_content_reader(xml);
        loop {
            match reader.read_event().unwrap() {
                quick_xml::events::Event::Start(ref e) if e.local_name().as_ref() == b"drawing" => {
                    break;
                }
                quick_xml::events::Event::Eof => panic!("no drawing"),
                _ => {}
            }
        }
        let info = parse_drawing(&mut reader).unwrap().expect("drawing");
        let shape = info.shape.expect("shape");
        assert_eq!(shape.kind, crate::docx::ShapeKind::Line);
        assert_eq!(shape.stroke_rgb, Some((0xFF, 0x00, 0x00)));
        assert_eq!(shape.stroke_w_emu, Some(9525));
    }

    #[test]
    fn section_properties() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>Content</w:t></w:r></w:p>
    <w:sectPr>
      <w:pgSz w:w="12240" w:h="15840"/>
      <w:pgMar w:top="1440" w:bottom="1440" w:left="1800" w:right="1800"/>
    </w:sectPr>
  </w:body>
</w:document>"#;
        let data = make_minimal_docx(xml);
        let doc = DocxDocument::from_reader(Cursor::new(data)).unwrap();
        assert_eq!(doc.sections.len(), 1);
        let sect = &doc.sections[0];
        let ps = sect.page_size.as_ref().unwrap();
        assert_eq!(ps.width.0, 12240);
        assert_eq!(ps.height.0, 15840);
        let margins = sect.margins.as_ref().unwrap();
        assert_eq!(margins.left.0, 1800);
    }

    // ── strip_embedded_font_filename ────────────────────────────────────

    #[test]
    fn strip_embedded_font_writer_convention() {
        // Writer convention: font_<n>_<face>.<ext>
        assert_eq!(
            strip_embedded_font_filename("font_4_TeXGyreTermesX-Regular.ttf"),
            "TeXGyreTermesX-Regular"
        );
        assert_eq!(
            strip_embedded_font_filename("font_1_NewTXBMI.ttf"),
            "NewTXBMI"
        );
        assert_eq!(
            strip_embedded_font_filename("font_12_DejaVuSans.otf"),
            "DejaVuSans"
        );
    }

    #[test]
    fn strip_embedded_font_no_prefix_keeps_stem() {
        // No `font_<n>_` prefix → return the stem unchanged.
        assert_eq!(strip_embedded_font_filename("Arial.ttf"), "Arial");
        assert_eq!(strip_embedded_font_filename("MyFont.otf"), "MyFont");
    }

    #[test]
    fn strip_embedded_font_no_extension() {
        // No extension → use the whole input.
        assert_eq!(strip_embedded_font_filename("font_1_Calibri"), "Calibri");
        assert_eq!(strip_embedded_font_filename("Calibri"), "Calibri");
    }

    #[test]
    fn strip_embedded_font_non_digit_prefix_keeps_stem() {
        // `font_xxx_<face>` where xxx isn't digits → don't strip.
        assert_eq!(
            strip_embedded_font_filename("font_abc_Foo.ttf"),
            "font_abc_Foo"
        );
    }

    #[test]
    fn strip_embedded_font_alphabetic_face_preserved() {
        // Regression: greedy trim_end_matches(alphabetic) used to eat
        // the face name. Verify a face with trailing alphabetic chars
        // survives intact.
        assert_eq!(
            strip_embedded_font_filename("font_4_TeXGyreTermesX-Bold.ttf"),
            "TeXGyreTermesX-Bold"
        );
    }

    #[test]
    fn strip_embedded_font_empty() {
        assert_eq!(strip_embedded_font_filename(""), "");
    }

    #[test]
    fn strip_embedded_font_no_face_after_prefix() {
        // `font_<n>_` with nothing after the underscore → empty face.
        // Caller of this helper falls back to the full basename.
        assert_eq!(strip_embedded_font_filename("font_5_.ttf"), "");
    }
}
