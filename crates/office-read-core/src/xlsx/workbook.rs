use quick_xml::events::Event;

use crate::core::xml;

#[derive(Debug, Clone)]
pub struct WorkbookInfo {
    pub sheets: Vec<SheetInfo>,
    pub date1904: bool,
}

#[derive(Debug, Clone)]
pub struct SheetInfo {
    pub name: String,
    pub rel_id: String,
}

impl WorkbookInfo {
    pub fn parse(xml_data: &[u8]) -> crate::core::Result<Self> {
        let mut reader = xml::make_fast_reader(xml_data);
        let mut sheets = Vec::new();
        let mut date1904 = false;

        loop {
            crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
            match reader.read_event()? {
                Event::Start(ref element) if element.local_name().as_ref() == b"sheet" => {
                    push_sheet(&mut sheets, element)?;
                    reader.read_to_end(element.to_end().name())?;
                }
                Event::Empty(ref element) if element.local_name().as_ref() == b"sheet" => {
                    push_sheet(&mut sheets, element)?;
                }
                Event::Start(ref element) if element.local_name().as_ref() == b"workbookPr" => {
                    date1904 = parse_date_system(element)?;
                    reader.read_to_end(element.to_end().name())?;
                }
                Event::Empty(ref element) if element.local_name().as_ref() == b"workbookPr" => {
                    date1904 = parse_date_system(element)?;
                }
                Event::Eof => break,
                _ => {}
            }
        }

        if sheets.is_empty() {
            return Err(crate::core::Error::MalformedXml(
                "workbook has no worksheets".to_owned(),
            ));
        }
        Ok(Self { sheets, date1904 })
    }
}

fn push_sheet(
    sheets: &mut Vec<SheetInfo>,
    element: &quick_xml::events::BytesStart,
) -> crate::core::Result<()> {
    let name = xml::required_attr_str(element, b"name")?;
    let rel_id = match xml::optional_attr_str(element, b"r:id")? {
        Some(value) => value,
        None => xml::optional_prefixed_attr_str(element, b"id")?.ok_or_else(|| {
            crate::core::Error::MissingAttribute {
                element: "sheet".to_owned(),
                attr: "r:id".to_owned(),
            }
        })?,
    };
    crate::budget::check_model_items("office_xlsx_sheets", sheets.len().saturating_add(1))
        .map_err(crate::map_office_error_to_core)?;
    crate::budget::check_model_text_bytes(
        "office_xlsx_sheet_metadata_bytes",
        name.len().saturating_add(rel_id.len()),
    )
    .map_err(crate::map_office_error_to_core)?;
    sheets.push(SheetInfo {
        name: name.into_owned(),
        rel_id: rel_id.into_owned(),
    });
    Ok(())
}

fn parse_date_system(element: &quick_xml::events::BytesStart) -> crate::core::Result<bool> {
    Ok(xml::optional_attr_str(element, b"date1904")?
        .is_some_and(|value| matches!(value.as_ref(), "1" | "true")))
}
