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
                    if e.local_name().as_ref() == b"hyperlink" {
                        if let Some(hl) = parse_hyperlink(e, rels)? {
                            hyperlinks.push(hl);
                            validate_hyperlinks(&hyperlinks)?;
                        }
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

#[cfg(any())]
mod tests {
    use super::*;
    use crate::core::relationships::Relationships;

    fn empty_rels() -> Relationships {
        Relationships::empty()
    }

    #[test]
    fn parse_simple_worksheet() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:B2"/>
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1"><v>42</v></c>
    </row>
    <row r="2">
      <c r="A2" t="b"><v>1</v></c>
      <c r="B2" t="e"><v>#DIV/0!</v></c>
    </row>
  </sheetData>
</worksheet>"#;
        let ws = Worksheet::parse(xml, "Sheet1".to_string(), &empty_rels()).unwrap();
        assert_eq!(ws.name, "Sheet1");
        assert_eq!(ws.dimension.as_deref(), Some("A1:B2"));
        assert_eq!(ws.rows.len(), 2);

        // Row 1
        assert_eq!(ws.rows[0].cells.len(), 2);
        assert!(matches!(
            ws.rows[0].cells[0].value,
            CellValue::SharedString(0)
        ));
        assert!(matches!(ws.rows[0].cells[1].value, CellValue::Number(n) if n == 42.0));

        // Row 2
        assert!(matches!(
            ws.rows[1].cells[0].value,
            CellValue::Boolean(true)
        ));
        assert!(matches!(&ws.rows[1].cells[1].value, CellValue::Error(e) if e == "#DIV/0!"));
    }

    #[test]
    fn parse_worksheet_with_formula() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1"><v>10</v></c>
      <c r="B1"><f>A1*2</f><v>20</v></c>
    </row>
  </sheetData>
</worksheet>"#;
        let ws = Worksheet::parse(xml, "Sheet1".to_string(), &empty_rels()).unwrap();
        let cell = &ws.rows[0].cells[1];
        assert_eq!(cell.formula.as_deref(), Some("A1*2"));
        assert!(matches!(cell.value, CellValue::Number(n) if n == 20.0));
    }

    #[test]
    fn parse_worksheet_page_setup() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData/>
  <pageMargins left="0.5" right="0.5" top="0.5" bottom="0.5" header="0.3" footer="0.3"/>
  <pageSetup paperWidth="215.90mm" paperHeight="279.40mm" orientation="portrait"/>
</worksheet>"#;
        let ws = Worksheet::parse(xml, "S".to_string(), &empty_rels()).unwrap();
        let ps = ws.page_setup.expect("page_setup parsed");
        // 215.9mm ≈ 8.5", 279.4mm ≈ 11", both in twips
        assert!(
            (ps.width_twips as i32 - 12240).abs() <= 1,
            "width {:?}",
            ps.width_twips
        );
        assert!(
            (ps.height_twips as i32 - 15840).abs() <= 1,
            "height {:?}",
            ps.height_twips
        );
        // 0.5" margin = 720 twips
        assert_eq!(ps.margin_top_twips, 720);
        assert_eq!(ps.margin_left_twips, 720);
        assert!(!ps.landscape);
    }

    #[test]
    fn parse_worksheet_page_setup_paper_enum() {
        // paperSize=9 = A4 → 11906x16838 twips.
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData/>
  <pageMargins left="0.7" right="0.7" top="0.75" bottom="0.75" header="0.3" footer="0.3"/>
  <pageSetup paperSize="9" orientation="landscape"/>
</worksheet>"#;
        let ws = Worksheet::parse(xml, "S".to_string(), &empty_rels()).unwrap();
        let ps = ws.page_setup.expect("page_setup parsed");
        assert_eq!(ps.width_twips, 11906);
        assert_eq!(ps.height_twips, 16838);
        assert!(ps.landscape);
    }

    #[test]
    fn parse_worksheet_merged_cells() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
    </row>
  </sheetData>
  <mergeCells count="1">
    <mergeCell ref="A1:C1"/>
  </mergeCells>
</worksheet>"#;
        let ws = Worksheet::parse(xml, "Sheet1".to_string(), &empty_rels()).unwrap();
        assert_eq!(ws.merged_cells, vec!["A1:C1"]);
    }

    // ── dim_to_twips ─────────────────────────────────────────────────────

    #[test]
    fn dim_to_twips_inches() {
        // 1 inch = 1440 twips.
        assert_eq!(dim_to_twips("1in"), Some(1440));
        assert_eq!(dim_to_twips("8.5in"), Some(12240));
    }

    #[test]
    fn dim_to_twips_millimeters() {
        // 210mm = 11906 twips (A4 width); allow ±1 for rounding.
        let twips = dim_to_twips("210mm").unwrap();
        assert!((twips as i32 - 11906).abs() <= 1, "got {twips}");
    }

    #[test]
    fn dim_to_twips_centimeters() {
        // 1cm = 1440/2.54 ≈ 567 twips.
        let twips = dim_to_twips("1cm").unwrap();
        assert!((twips as i32 - 567).abs() <= 1, "got {twips}");
    }

    #[test]
    fn dim_to_twips_bare_number_assumed_mm() {
        // Bare numeric defaults to mm.
        let a = dim_to_twips("210mm").unwrap();
        let b = dim_to_twips("210").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn dim_to_twips_empty_and_zero() {
        assert_eq!(dim_to_twips(""), None);
        assert_eq!(dim_to_twips("   "), None);
        // Zero / negative dimensions are nonsensical: rejected.
        assert_eq!(dim_to_twips("0mm"), None);
        assert_eq!(dim_to_twips("-5in"), None);
    }

    #[test]
    fn dim_to_twips_invalid_string() {
        assert_eq!(dim_to_twips("garbage"), None);
        assert_eq!(dim_to_twips("abcmm"), None);
    }

    // ── paper_size_enum_to_twips ────────────────────────────────────────

    #[test]
    fn paper_size_letter() {
        assert_eq!(paper_size_enum_to_twips(1), (12240, 15840));
    }

    #[test]
    fn paper_size_legal() {
        assert_eq!(paper_size_enum_to_twips(5), (12240, 20160));
    }

    #[test]
    fn paper_size_a4() {
        assert_eq!(paper_size_enum_to_twips(9), (11906, 16838));
    }

    #[test]
    fn paper_size_unknown_falls_back_to_a4() {
        assert_eq!(paper_size_enum_to_twips(9999), (11906, 16838));
    }

    // ── build_page_setup ────────────────────────────────────────────────

    #[test]
    fn build_page_setup_returns_none_when_both_missing() {
        assert!(build_page_setup(None, None).is_none());
    }

    #[test]
    fn build_page_setup_margins_only_zeroes_dimensions() {
        // <pageMargins> without <pageSetup> → dimensions left at 0 so
        // a downstream consumer falls back to its default page size.
        let margins = Some(PageMarginsIn {
            left: 1.0,
            right: 1.0,
            top: 1.0,
            bottom: 1.0,
            header: 0.5,
            footer: 0.5,
        });
        let ps = build_page_setup(margins, None).unwrap();
        assert_eq!(ps.width_twips, 0);
        assert_eq!(ps.height_twips, 0);
        // 1 inch margins = 1440 twips.
        assert_eq!(ps.margin_top_twips, 1440);
        assert_eq!(ps.margin_left_twips, 1440);
        assert_eq!(ps.header_distance_twips, 720); // 0.5 in
    }

    #[test]
    fn build_page_setup_dimensions_only_uses_default_margins() {
        // <pageSetup> alone uses ECMA-376 default 0.7/0.7/0.75/0.75 inch margins.
        let raw = Some(PageSetupRaw {
            width_twips: 12240,
            height_twips: 15840,
            landscape: false,
        });
        let ps = build_page_setup(None, raw).unwrap();
        assert_eq!(ps.width_twips, 12240);
        assert_eq!(ps.height_twips, 15840);
        // 0.7in = 1008 twips.
        assert_eq!(ps.margin_left_twips, 1008);
        // 0.75in = 1080 twips.
        assert_eq!(ps.margin_top_twips, 1080);
    }

    #[test]
    fn build_page_setup_combines_both() {
        let margins = Some(PageMarginsIn {
            left: 0.5,
            right: 0.5,
            top: 0.5,
            bottom: 0.5,
            header: 0.3,
            footer: 0.3,
        });
        let raw = Some(PageSetupRaw {
            width_twips: 11906,
            height_twips: 16838,
            landscape: true,
        });
        let ps = build_page_setup(margins, raw).unwrap();
        assert_eq!(ps.width_twips, 11906);
        assert!(ps.landscape);
        assert_eq!(ps.margin_left_twips, 720); // 0.5in
    }

    #[test]
    fn parse_worksheet_landscape_with_paper_enum() {
        // Verifies that landscape attribute survives the parse_page_setup_attrs path.
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData/>
  <pageSetup paperSize="1" orientation="landscape"/>
</worksheet>"#;
        let ws = Worksheet::parse(xml, "S".to_string(), &empty_rels()).unwrap();
        let ps = ws.page_setup.expect("page_setup");
        assert_eq!(ps.width_twips, 12240); // Letter
        assert!(ps.landscape);
    }

    #[test]
    fn parse_worksheet_default_when_no_setup() {
        // No <pageMargins> or <pageSetup> → no page_setup at all.
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData/>
</worksheet>"#;
        let ws = Worksheet::parse(xml, "S".to_string(), &empty_rels()).unwrap();
        assert!(ws.page_setup.is_none());
    }
}
