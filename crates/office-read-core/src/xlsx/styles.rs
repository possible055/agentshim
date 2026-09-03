use std::collections::HashMap;

use quick_xml::events::Event;

use crate::core::xml;

#[derive(Debug, Clone)]
pub struct StyleSheet {
    number_formats: HashMap<u32, String>,
    cell_formats: Vec<CellFormat>,
}

#[derive(Debug, Clone, Copy)]
struct CellFormat {
    number_format_id: u32,
}

impl StyleSheet {
    pub fn parse(xml_data: &[u8]) -> crate::core::Result<Self> {
        let mut reader = xml::make_fast_reader(xml_data);
        let mut number_formats = HashMap::new();
        let mut cell_formats = Vec::new();

        loop {
            match reader.read_event()? {
                Event::Start(ref element) => match element.local_name().as_ref() {
                    b"numFmts" => number_formats = parse_number_formats(&mut reader)?,
                    b"cellXfs" => cell_formats = parse_cell_formats(&mut reader)?,
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
        }

        Ok(Self {
            number_formats,
            cell_formats,
        })
    }

    pub fn number_format_for(&self, style_index: u32) -> Option<&str> {
        let format_id = self
            .cell_formats
            .get(style_index as usize)?
            .number_format_id;
        self.number_formats.get(&format_id).map(String::as_str)
    }

    pub fn number_format_id_for(&self, style_index: u32) -> Option<u32> {
        self.cell_formats
            .get(style_index as usize)
            .map(|format| format.number_format_id)
    }

    pub fn contains_style(&self, style_index: u32) -> bool {
        self.cell_formats.get(style_index as usize).is_some()
    }
}

fn parse_number_formats(
    reader: &mut quick_xml::Reader<&[u8]>,
) -> crate::core::Result<HashMap<u32, String>> {
    let mut formats = HashMap::new();
    loop {
        match reader.read_event()? {
            Event::Start(ref element) | Event::Empty(ref element)
                if element.local_name().as_ref() == b"numFmt" =>
            {
                let id = xml::required_attr_str(element, b"numFmtId")?.parse()?;
                let code = xml::required_attr_str(element, b"formatCode")?;
                crate::budget::check_model_items(
                    "office_xlsx_number_formats",
                    formats.len().saturating_add(1),
                )
                .map_err(crate::map_office_error_to_core)?;
                crate::budget::check_model_text_bytes(
                    "office_xlsx_number_format_bytes",
                    code.len(),
                )
                .map_err(crate::map_office_error_to_core)?;
                formats.insert(id, code.into_owned());
            }
            Event::End(ref element) if element.local_name().as_ref() == b"numFmts" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated number formats".to_owned(),
                ));
            }
            _ => {}
        }
    }
    Ok(formats)
}

fn parse_cell_formats(
    reader: &mut quick_xml::Reader<&[u8]>,
) -> crate::core::Result<Vec<CellFormat>> {
    let mut formats = Vec::new();
    loop {
        match reader.read_event()? {
            Event::Start(ref element) if element.local_name().as_ref() == b"xf" => {
                push_cell_format(&mut formats, element)?;
                reader.read_to_end(element.to_end().name())?;
            }
            Event::Empty(ref element) if element.local_name().as_ref() == b"xf" => {
                push_cell_format(&mut formats, element)?;
            }
            Event::End(ref element) if element.local_name().as_ref() == b"cellXfs" => break,
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated cell formats".to_owned(),
                ));
            }
            _ => {}
        }
    }
    Ok(formats)
}

fn push_cell_format(
    formats: &mut Vec<CellFormat>,
    element: &quick_xml::events::BytesStart,
) -> crate::core::Result<()> {
    crate::budget::check_model_items("office_xlsx_cell_formats", formats.len().saturating_add(1))
        .map_err(crate::map_office_error_to_core)?;
    let number_format_id = xml::optional_attr_str(element, b"numFmtId")?
        .map(|value| value.parse::<u32>().map_err(crate::core::Error::from))
        .transpose()?
        .unwrap_or(0);
    formats.push(CellFormat { number_format_id });
    Ok(())
}
