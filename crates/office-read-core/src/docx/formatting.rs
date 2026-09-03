use quick_xml::events::{BytesStart, Event};

use crate::core::xml;

#[derive(Debug, Clone, Default)]
pub struct RunProperties {
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub strike: Option<bool>,
    pub dstrike: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct ParagraphProperties {
    pub style_id: Option<String>,
    pub numbering_ref: Option<NumberingRef>,
    pub outline_level: Option<u8>,
    pub section_properties: Option<super::SectionProperties>,
}

#[derive(Debug, Clone)]
pub struct NumberingRef {
    pub num_id: u32,
    pub ilvl: u8,
}

pub(crate) fn parse_run_properties_fast(
    reader: &mut quick_xml::Reader<&[u8]>,
) -> crate::core::Result<RunProperties> {
    let mut properties = RunProperties::default();

    loop {
        match reader.read_event()? {
            Event::Start(ref event) => {
                match event.local_name().as_ref() {
                    b"b" => properties.bold = Some(parse_toggle(event)),
                    b"i" => properties.italic = Some(parse_toggle(event)),
                    b"strike" => properties.strike = Some(parse_toggle(event)),
                    b"dstrike" => properties.dstrike = Some(parse_toggle(event)),
                    _ => {}
                }
                xml::skip_element_fast(reader)?;
            }
            Event::Empty(ref event) => match event.local_name().as_ref() {
                b"b" => properties.bold = Some(parse_toggle(event)),
                b"i" => properties.italic = Some(parse_toggle(event)),
                b"strike" => properties.strike = Some(parse_toggle(event)),
                b"dstrike" => properties.dstrike = Some(parse_toggle(event)),
                _ => {}
            },
            Event::End(ref event) if event.local_name().as_ref() == b"rPr" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated run properties".to_string(),
                ));
            }
            _ => {}
        }
    }

    Ok(properties)
}

pub(crate) fn parse_paragraph_properties_fast(
    reader: &mut quick_xml::Reader<&[u8]>,
) -> crate::core::Result<ParagraphProperties> {
    let mut properties = ParagraphProperties::default();

    loop {
        match reader.read_event()? {
            Event::Start(ref event) => match event.local_name().as_ref() {
                b"pStyle" => {
                    properties.style_id =
                        xml::optional_attr_str(event, b"w:val")?.map(|value| value.into_owned());
                    xml::skip_element_fast(reader)?;
                }
                b"numPr" => {
                    properties.numbering_ref = Some(parse_numbering_reference(reader)?);
                }
                b"outlineLvl" => {
                    properties.outline_level = parse_u8_attribute(event, b"w:val")?;
                    xml::skip_element_fast(reader)?;
                }
                b"sectPr" => {
                    properties.section_properties =
                        Some(super::parse_section_properties(reader, event)?);
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::Empty(ref event) => match event.local_name().as_ref() {
                b"pStyle" => {
                    properties.style_id =
                        xml::optional_attr_str(event, b"w:val")?.map(|value| value.into_owned());
                }
                b"outlineLvl" => {
                    properties.outline_level = parse_u8_attribute(event, b"w:val")?;
                }
                _ => {}
            },
            Event::End(ref event) if event.local_name().as_ref() == b"pPr" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated paragraph properties".to_string(),
                ));
            }
            _ => {}
        }
    }

    Ok(properties)
}

fn parse_numbering_reference(
    reader: &mut quick_xml::Reader<&[u8]>,
) -> crate::core::Result<NumberingRef> {
    let mut num_id = None;
    let mut ilvl = None;

    loop {
        match reader.read_event()? {
            Event::Start(ref event) => {
                match event.local_name().as_ref() {
                    b"numId" => num_id = parse_u32_attribute(event, b"w:val")?,
                    b"ilvl" => ilvl = parse_u8_attribute(event, b"w:val")?,
                    _ => {}
                }
                xml::skip_element_fast(reader)?;
            }
            Event::Empty(ref event) => match event.local_name().as_ref() {
                b"numId" => num_id = parse_u32_attribute(event, b"w:val")?,
                b"ilvl" => ilvl = parse_u8_attribute(event, b"w:val")?,
                _ => {}
            },
            Event::End(ref event) if event.local_name().as_ref() == b"numPr" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated numbering reference".to_string(),
                ));
            }
            _ => {}
        }
    }

    Ok(NumberingRef {
        num_id: num_id.ok_or_else(|| crate::core::Error::MissingAttribute {
            element: "numPr/numId".to_string(),
            attr: "w:val".to_string(),
        })?,
        ilvl: ilvl.unwrap_or(0),
    })
}

fn parse_toggle(event: &BytesStart) -> bool {
    xml::parse_toggle(event, b"w:val")
}

fn parse_u8_attribute(event: &BytesStart, name: &[u8]) -> crate::core::Result<Option<u8>> {
    xml::optional_attr_str(event, name)?
        .map(|value| value.parse().map_err(crate::core::Error::from))
        .transpose()
}

fn parse_u32_attribute(event: &BytesStart, name: &[u8]) -> crate::core::Result<Option<u32>> {
    xml::optional_attr_str(event, name)?
        .map(|value| value.parse().map_err(crate::core::Error::from))
        .transpose()
}
