use std::collections::HashMap;

use quick_xml::events::Event;

use crate::core::xml;

#[derive(Debug, Clone, Default)]
pub struct StyleSheet {
    styles: HashMap<String, Style>,
}

#[derive(Debug, Clone)]
struct Style {
    based_on: Option<String>,
    outline_level: Option<u8>,
}

impl StyleSheet {
    pub fn parse(xml_data: &[u8]) -> crate::core::Result<Self> {
        let mut reader = xml::make_fast_reader(xml_data);
        let mut styles = HashMap::new();
        loop {
            crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
            match reader.read_event()? {
                Event::Start(ref element) if element.local_name().as_ref() == b"style" => {
                    let style_id = xml::required_attr_str(element, b"w:styleId")?;
                    let style = parse_style(&mut reader)?;
                    crate::budget::charge_model_items("office_docx_styles", 1)
                        .map_err(crate::map_office_error_to_core)?;
                    crate::budget::charge_model_text(
                        "office_docx_style_bytes",
                        style_id
                            .len()
                            .saturating_add(style.based_on.as_ref().map_or(0, String::len)),
                    )
                    .map_err(crate::map_office_error_to_core)?;
                    if styles.insert(style_id.into_owned(), style).is_some() {
                        return Err(crate::core::Error::MalformedXml(
                            "duplicate DOCX style id".to_owned(),
                        ));
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(Self { styles })
    }

    pub fn resolve_outline_level(&self, style_id: &str) -> Option<u8> {
        let mut current_id = Some(style_id);
        let mut visited = std::collections::HashSet::new();
        while let Some(id) = current_id {
            if !visited.insert(id) {
                return None;
            }
            let style = self.styles.get(id)?;
            if let Some(level) = style.outline_level {
                return Some(level);
            }
            current_id = style.based_on.as_deref();
        }
        None
    }
}

fn parse_style(reader: &mut quick_xml::Reader<&[u8]>) -> crate::core::Result<Style> {
    let mut based_on = None;
    let mut outline_level = None;
    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref element) if element.local_name().as_ref() == b"pPr" => {
                outline_level = parse_outline_level(reader)?;
            }
            Event::Start(ref element) if element.local_name().as_ref() == b"basedOn" => {
                based_on =
                    xml::optional_attr_str(element, b"w:val")?.map(|value| value.into_owned());
                reader.read_to_end(element.to_end().name())?;
            }
            Event::Empty(ref element) if element.local_name().as_ref() == b"basedOn" => {
                based_on =
                    xml::optional_attr_str(element, b"w:val")?.map(|value| value.into_owned());
            }
            Event::Start(_) => xml::skip_element_fast(reader)?,
            Event::End(ref element) if element.local_name().as_ref() == b"style" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated DOCX style".to_owned(),
                ));
            }
            _ => {}
        }
    }
    Ok(Style {
        based_on,
        outline_level,
    })
}

fn parse_outline_level(reader: &mut quick_xml::Reader<&[u8]>) -> crate::core::Result<Option<u8>> {
    let mut level = None;
    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref element) | Event::Empty(ref element)
                if element.local_name().as_ref() == b"outlineLvl" =>
            {
                level = xml::optional_attr_str(element, b"w:val")?
                    .map(|value| value.parse::<u8>().map_err(crate::core::Error::from))
                    .transpose()?;
            }
            Event::Start(_) => xml::skip_element_fast(reader)?,
            Event::End(ref element) if element.local_name().as_ref() == b"pPr" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated style paragraph properties".to_owned(),
                ));
            }
            _ => {}
        }
    }
    Ok(level)
}
