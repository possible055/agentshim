use quick_xml::events::Event;

use crate::core::xml;

use super::cell::{Cell, CellRef, CellValue};

/// A parsed worksheet from `xl/worksheets/sheetN.xml`.
#[derive(Debug, Clone)]
pub struct Worksheet {
    /// Sheet display name.
    pub name: String,
    /// Data rows.
    pub rows: Vec<Row>,
    /// Hyperlinks defined on this sheet.
    pub hyperlinks: Vec<HyperlinkInfo>,
    /// Layout-preserving text shapes anchored on this worksheet via a
    /// DrawingML drawing part. Each entry is one `<xdr:sp>` carrying a
    /// single styled run — populated by the round-trip from
    /// `to_xlsx_bytes_layout`. Empty when the worksheet has no
    /// `<xdr:sp>` shapes (the common XLSX case).
    pub text_shapes: Vec<WorksheetTextShape>,
    /// Accessibility text from picture anchors, without raw media payloads.
    pub picture_alt_text: Vec<String>,
}

/// A text shape anchored on a worksheet via a DrawingML drawing part.
/// Mirrors `xlsx::write::SheetTextShape`.
#[derive(Debug, Clone)]
pub struct WorksheetTextShape {
    /// Text content of the shape.
    pub text: String,
    /// Bold weight.
    pub bold: bool,
    /// Italic style.
    pub italic: bool,
    /// X anchor in EMU.
    pub x_emu: i64,
    /// Y anchor in EMU.
    pub y_emu: i64,
}

/// A row from `<sheetData>`.
#[derive(Debug, Clone)]
pub struct Row {
    /// Cells in this row.
    pub cells: Vec<Cell>,
}

/// Hyperlink information from `<hyperlinks>`.
#[derive(Debug, Clone)]
pub struct HyperlinkInfo {
    /// Cell reference like "A1".
    pub cell_ref: String,
    /// Hyperlink destination.
    pub target: HyperlinkTarget,
}

/// Hyperlink target type.
#[derive(Debug, Clone)]
pub enum HyperlinkTarget {
    /// External URL.
    External(String),
    /// Internal sheet/cell location.
    Internal(String),
}

impl Worksheet {
    /// Parse a worksheet XML part.
    pub fn parse(
        xml_data: &[u8],
        name: String,
        rels: &crate::core::relationships::Relationships,
    ) -> crate::core::Result<Self> {
        // Use plain Reader (not NsReader) for performance — worksheet XML is always
        // in the SML namespace, so namespace resolution is unnecessary overhead.
        // This is the hot path: worksheets can have thousands of cells.
        let mut reader = xml::make_fast_reader(xml_data);
        let mut rows = Vec::new();
        let mut cell_count = 0_usize;
        let mut hyperlinks = Vec::new();
        let mut last_row_index = None;

        loop {
            crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
            match reader.read_event()? {
                Event::Start(ref e) => match e.local_name().as_ref() {
                    b"row" => {
                        let (row_index, row) = parse_row_fast(&mut reader, e)?;
                        if last_row_index.is_some_and(|previous| row_index <= previous) {
                            return Err(crate::core::Error::MalformedXml(
                                "worksheet rows are duplicated or out of order".to_owned(),
                            ));
                        }
                        last_row_index = Some(row_index);
                        cell_count = cell_count.saturating_add(row.cells.len());
                        crate::budget::check_model_items("office_xlsx_cells", cell_count)
                            .map_err(crate::map_office_error_to_core)?;
                        rows.push(row);
                        crate::budget::check_model_items("office_xlsx_rows", rows.len())
                            .map_err(crate::map_office_error_to_core)?;
                    }
                    b"hyperlink" => {
                        if let Some(hl) = parse_hyperlink(e, rels)? {
                            hyperlinks.push(hl);
                            validate_hyperlinks(&hyperlinks)?;
                        }
                        reader.read_to_end(e.to_end().name())?;
                    }
                    _ => {}
                },
                Event::Empty(ref e) => {
                    if e.local_name().as_ref() == b"hyperlink"
                        && let Some(hl) = parse_hyperlink(e, rels)?
                    {
                        hyperlinks.push(hl);
                        validate_hyperlinks(&hyperlinks)?;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }

        Ok(Worksheet {
            name,
            rows,
            hyperlinks,
            text_shapes: Vec::new(),
            picture_alt_text: Vec::new(),
        })
    }
}

fn parse_hyperlink(
    e: &quick_xml::events::BytesStart,
    rels: &crate::core::relationships::Relationships,
) -> crate::core::Result<Option<HyperlinkInfo>> {
    let cell_ref = xml::required_attr_str(e, b"ref")?.into_owned();
    CellRef::parse(&cell_ref).ok_or_else(|| {
        crate::core::Error::MalformedXml("invalid hyperlink reference".to_owned())
    })?;
    // r:id → external hyperlink via relationships
    let r_id = xml::optional_attr_str(e, b"r:id")?;
    let location = xml::optional_attr_str(e, b"location")?;

    let target = if let Some(rid) = r_id {
        let rel = rels
            .get_by_id(&rid)
            .ok_or_else(|| crate::core::Error::RelationshipNotFound(rid.into_owned()))?;
        if rel.target_mode != crate::core::relationships::TargetMode::External {
            return Err(crate::core::Error::MalformedXml(
                "worksheet hyperlink relationship must be external".to_owned(),
            ));
        }
        HyperlinkTarget::External(rel.target.clone())
    } else if let Some(loc) = location {
        HyperlinkTarget::Internal(loc.into_owned())
    } else {
        return Err(crate::core::Error::MalformedXml(
            "hyperlink has no target".to_owned(),
        ));
    };

    Ok(Some(HyperlinkInfo { cell_ref, target }))
}

fn validate_hyperlinks(hyperlinks: &[HyperlinkInfo]) -> crate::core::Result<()> {
    crate::budget::check_model_items("office_xlsx_hyperlinks", hyperlinks.len())
        .map_err(crate::map_office_error_to_core)?;
    let bytes = hyperlinks.iter().try_fold(0_usize, |total, hyperlink| {
        let target_len = match &hyperlink.target {
            HyperlinkTarget::External(target) | HyperlinkTarget::Internal(target) => target.len(),
        };
        total
            .checked_add(hyperlink.cell_ref.len())
            .and_then(|value| value.checked_add(target_len))
            .ok_or(crate::core::Error::ResourceLimit {
                resource: "office_xlsx_hyperlink_bytes",
                limit: u64::MAX,
                observed: u64::MAX,
            })
    })?;
    crate::budget::check_model_text_bytes("office_xlsx_hyperlink_bytes", bytes)
        .map_err(crate::map_office_error_to_core)
}

/// Fast row parser using plain Reader (no namespace resolution).
fn parse_row_fast(
    reader: &mut quick_xml::Reader<&[u8]>,
    start: &quick_xml::events::BytesStart,
) -> crate::core::Result<(u32, Row)> {
    let row_index = xml::required_attr_str(start, b"r")?
        .parse::<u32>()?
        .checked_sub(1)
        .filter(|row| *row < 1_048_576)
        .ok_or_else(|| crate::core::Error::MalformedXml("invalid row index".to_owned()))?;
    let mut cells = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => {
                if e.local_name().as_ref() == b"c" {
                    cells.push(parse_cell_fast(reader, e)?);
                } else {
                    reader.read_to_end(e.to_end().name())?;
                }
            }
            Event::Empty(ref e) if e.local_name().as_ref() == b"c" => {
                cells.push(parse_empty_cell(e)?);
            }
            Event::End(ref e) if e.local_name().as_ref() == b"row" => {
                break;
            }
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated worksheet row".to_owned(),
                ));
            }
            _ => {}
        }
    }

    cells.sort_unstable_by_key(|cell| cell.reference.col);
    if cells
        .windows(2)
        .any(|pair| pair[0].reference.col == pair[1].reference.col)
    {
        return Err(crate::core::Error::MalformedXml(
            "duplicate cell reference in row".to_owned(),
        ));
    }
    if cells.iter().any(|cell| cell.reference.row != row_index) {
        return Err(crate::core::Error::MalformedXml(
            "cell reference does not match containing row".to_owned(),
        ));
    }
    Ok((row_index, Row { cells }))
}

fn parse_empty_cell(e: &quick_xml::events::BytesStart) -> crate::core::Result<Cell> {
    let ref_str = xml::optional_attr_str(e, b"r")?
        .map(|v| v.into_owned())
        .unwrap_or_default();
    let reference = CellRef::parse(&ref_str)
        .ok_or_else(|| crate::core::Error::MalformedXml("invalid cell reference".to_owned()))?;
    let style_index = parse_style_index(e)?;

    Ok(Cell {
        reference,
        value: CellValue::Empty,
        style_index,
    })
}

/// Fast cell parser using plain Reader (no namespace resolution).
fn parse_cell_fast(
    reader: &mut quick_xml::Reader<&[u8]>,
    start: &quick_xml::events::BytesStart,
) -> crate::core::Result<Cell> {
    let ref_str = xml::optional_attr_str(start, b"r")?
        .map(|v| v.into_owned())
        .unwrap_or_default();
    let reference = CellRef::parse(&ref_str)
        .ok_or_else(|| crate::core::Error::MalformedXml("invalid cell reference".to_owned()))?;

    let cell_type = xml::optional_attr_str(start, b"t")?.map(|v| v.into_owned());
    let style_index = parse_style_index(start)?;

    let mut raw_value: Option<String> = None;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"v" => {
                    raw_value = Some(read_text_fast(reader)?);
                }
                b"f" => {
                    let _ = read_text_fast(reader)?;
                }
                b"is" => {
                    raw_value = Some(parse_inline_string_fast(reader)?);
                }
                _ => {
                    reader.read_to_end(e.to_end().name())?;
                }
            },
            Event::Empty(ref e) if e.local_name().as_ref() == b"f" => {}
            Event::End(ref e) if e.local_name().as_ref() == b"c" => {
                break;
            }
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated worksheet cell".to_owned(),
                ));
            }
            _ => {}
        }
    }

    let value = match cell_type.as_deref() {
        Some("s") => {
            let index = raw_value
                .as_deref()
                .ok_or_else(|| {
                    crate::core::Error::MalformedXml("shared string cell has no index".to_owned())
                })?
                .parse::<u32>()?;
            CellValue::SharedString(index)
        }
        Some("str") | Some("inlineStr") => match raw_value {
            Some(s) => CellValue::String(s),
            None => CellValue::Empty,
        },
        Some("b") => match raw_value.as_deref() {
            Some("1") | Some("true") => CellValue::Boolean(true),
            Some("0") | Some("false") => CellValue::Boolean(false),
            _ => {
                return Err(crate::core::Error::MalformedXml(
                    "invalid boolean cell value".to_owned(),
                ));
            }
        },
        Some("e") => match raw_value {
            Some(s) => CellValue::Error(s),
            None => CellValue::Error(String::new()),
        },
        _ => match raw_value {
            Some(s) => CellValue::Number(fast_float2::parse::<f64, _>(&s).map_err(|_| {
                crate::core::Error::MalformedXml("invalid numeric cell value".to_owned())
            })?),
            None => CellValue::Empty,
        },
    };

    Ok(Cell {
        reference,
        value,
        style_index,
    })
}

fn parse_style_index(element: &quick_xml::events::BytesStart) -> crate::core::Result<Option<u32>> {
    xml::optional_attr_str(element, b"s")?
        .map(|value| value.parse::<u32>().map_err(crate::core::Error::from))
        .transpose()
}

/// Read text content of the current element using fast Reader.
fn read_text_fast(reader: &mut quick_xml::Reader<&[u8]>) -> crate::core::Result<String> {
    xml::read_text_content_fast(reader)
}

/// Fast inline string parser: `<is><t>text</t></is>` or `<is><r>...<t>text</t>...</r></is>`.
fn parse_inline_string_fast(reader: &mut quick_xml::Reader<&[u8]>) -> crate::core::Result<String> {
    let mut text = String::new();

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => {
                if e.local_name().as_ref() == b"t" {
                    text.push_str(&read_text_fast(reader)?);
                } else {
                    reader.read_to_end(e.to_end().name())?;
                }
            }
            Event::End(ref e) if e.local_name().as_ref() == b"is" => {
                break;
            }
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated inline string".to_owned(),
                ));
            }
            _ => {}
        }
    }

    Ok(text)
}
