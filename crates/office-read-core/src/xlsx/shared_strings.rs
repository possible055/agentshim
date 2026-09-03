use quick_xml::events::Event;

use crate::core::xml;

#[derive(Debug, Clone)]
pub struct SharedStringTable {
    strings: Vec<String>,
}

impl SharedStringTable {
    pub fn empty() -> Self {
        Self {
            strings: Vec::new(),
        }
    }

    pub fn parse(xml_data: &[u8]) -> crate::core::Result<Self> {
        let mut reader = quick_xml::Reader::from_reader(xml_data);
        reader.config_mut().check_end_names = false;
        reader.config_mut().check_comments = false;
        let mut strings = Vec::new();
        let mut text_bytes = 0_usize;

        loop {
            crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
            match reader.read_event()? {
                Event::Start(ref element) if element.local_name().as_ref() == b"si" => {
                    let text = parse_shared_string(&mut reader)?;
                    text_bytes = text_bytes.saturating_add(text.len());
                    crate::budget::check_model_items(
                        "office_xlsx_shared_strings",
                        strings.len().saturating_add(1),
                    )
                    .map_err(crate::map_office_error_to_core)?;
                    crate::budget::check_model_text_bytes(
                        "office_xlsx_shared_string_bytes",
                        text_bytes,
                    )
                    .map_err(crate::map_office_error_to_core)?;
                    strings.push(text);
                }
                Event::Eof => break,
                _ => {}
            }
        }

        Ok(Self { strings })
    }

    pub fn get(&self, index: u32) -> Option<&str> {
        self.strings.get(index as usize).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.strings.len()
    }
}

fn parse_shared_string(reader: &mut quick_xml::Reader<&[u8]>) -> crate::core::Result<String> {
    let mut text = String::new();
    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref element) => match element.local_name().as_ref() {
                b"t" => text.push_str(&xml::read_text_content_fast(reader)?),
                b"r" => text.push_str(&parse_rich_text_run(reader)?),
                _ => xml::skip_element_fast(reader)?,
            },
            Event::End(ref element) if element.local_name().as_ref() == b"si" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated shared string".to_owned(),
                ));
            }
            _ => {}
        }
    }
    Ok(text)
}

fn parse_rich_text_run(reader: &mut quick_xml::Reader<&[u8]>) -> crate::core::Result<String> {
    let mut text = String::new();
    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref element) if element.local_name().as_ref() == b"t" => {
                text.push_str(&xml::read_text_content_fast(reader)?);
            }
            Event::Start(_) => xml::skip_element_fast(reader)?,
            Event::End(ref element) if element.local_name().as_ref() == b"r" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated shared-string rich text run".to_owned(),
                ));
            }
            _ => {}
        }
    }
    Ok(text)
}
