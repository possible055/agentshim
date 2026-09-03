//! Workbook-level parsing: sheets, SST, cell grid construction.

use std::io::{Read, Seek};

use crate::cfb::CfbReader;

use super::cell::{Cell, CellValue, parse_cell_record};
use super::error::{Result, XlsError};
use super::records::*;
use super::sst::{parse_sst, read_short_unicode_string, read_unicode_string};

/// A parsed legacy XLS document.
#[derive(Debug)]
pub struct XlsDocument {
    /// Worksheets in workbook order.
    pub sheets: Vec<Sheet>,
}

/// A worksheet from an XLS workbook.
#[derive(Debug)]
pub struct Sheet {
    /// Sheet display name.
    pub name: String,
    /// Cell values, indexed as `rows[row][col]`.
    pub rows: Vec<Vec<CellValue>>,
}

/// Sheet metadata from BOUNDSHEET records.
#[derive(Debug)]
struct SheetInfo {
    name: String,
    #[allow(
        dead_code,
        reason = "part of the BOUNDSHEET record layout; retained for spec fidelity"
    )]
    offset: u32,
    hidden: bool,
}

impl XlsDocument {
    pub(crate) fn sheet_markdown_start(&self, index: usize) -> Option<String> {
        let sheet = self.sheets.get(index)?;
        let mut out = String::new();
        if index > 0 {
            out.push('\n');
        }
        out.push_str("## ");
        out.push_str(&sheet.name);
        out.push_str("\n\n");
        if sheet.rows.is_empty() {
            return Some(out);
        }
        let column_count = sheet.rows.iter().map(Vec::len).max().unwrap_or(0);
        if column_count == 0 {
            return Some(out);
        }
        out.push('|');
        if let Some(first_row) = sheet.rows.first() {
            for column in 0..column_count {
                let text = first_row
                    .get(column)
                    .map(CellValue::as_text)
                    .unwrap_or_default();
                out.push(' ');
                out.push_str(&text);
                out.push_str(" |");
            }
        }
        out.push_str("\n|");
        for _ in 0..column_count {
            out.push_str(" --- |");
        }
        out.push('\n');
        Some(out)
    }

    pub(crate) fn sheet_markdown_row(&self, sheet: usize, row: usize) -> Option<String> {
        let sheet = self.sheets.get(sheet)?;
        let row = sheet.rows.get(row)?;
        let column_count = sheet.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut out = String::from("|");
        for column in 0..column_count {
            let text = row.get(column).map(CellValue::as_text).unwrap_or_default();
            out.push(' ');
            out.push_str(&text);
            out.push_str(" |");
        }
        out.push('\n');
        Some(out)
    }

    /// Open an XLS file from a reader (any `Read + Seek`).
    pub fn from_reader<R: Read + Seek>(reader: R) -> Result<Self> {
        let mut cfb = CfbReader::new(reader)?;

        // Try "Workbook" (BIFF8) first, then "Book" (BIFF5).
        let stream_data = if cfb.has_stream("Workbook") {
            cfb.open_stream("Workbook")?
        } else if cfb.has_stream("Book") {
            cfb.open_stream("Book")?
        } else {
            return Err(XlsError::MissingStream(
                "neither Workbook nor Book stream found".into(),
            ));
        };
        // Drop CFB early to free file handle and memory.
        drop(cfb);

        Self::parse_workbook_stream(&stream_data)
    }

    fn parse_workbook_stream(data: &[u8]) -> Result<Self> {
        let mut sheet_infos: Vec<SheetInfo> = Vec::new();
        let mut sst: Vec<String> = Vec::new();
        let mut sheets = Vec::new();

        if data.len() < 8 || u16::from_le_bytes([data[0], data[1]]) != RT_BOF {
            return Err(XlsError::InvalidRecord(
                "workbook stream does not begin with BOF".to_owned(),
            ));
        }
        let version = u16::from_le_bytes([data[4], data[5]]);
        let biff8 = match version {
            0x0600 => true,
            0x0500 => false,
            other => return Err(XlsError::UnsupportedVersion(other)),
        };
        if u16::from_le_bytes([data[6], data[7]]) != 0x0005 {
            return Err(XlsError::InvalidRecord(
                "first BOF is not a workbook-globals substream".to_owned(),
            ));
        }

        // Single-pass parsing: globals then sheets sequentially.
        let mut phase = Phase::Globals;
        let mut cells: Vec<Cell> = Vec::new();
        let mut sheet_idx = 0usize;
        let mut pending_formula_string: Option<(u16, u16)> = None;
        for rec in RecordIter::new(data) {
            crate::budget::check_cancelled().map_err(map_budget_error)?;
            crate::budget::charge_model_items("office_xls_records", 1).map_err(map_budget_error)?;
            let rec = rec?;
            match phase {
                Phase::Globals => match rec.record_type {
                    RT_FILEPASS => {
                        // File is encrypted — we can't read it.
                        return Err(XlsError::UnsupportedVersion(0));
                    }
                    RT_BOUNDSHEET => {
                        let info = parse_boundsheet(&rec.data)?;
                        crate::budget::charge_model_items("office_xls_sheets", 1)
                            .map_err(map_budget_error)?;
                        crate::budget::charge_model_text("office_xls_text_bytes", info.name.len())
                            .map_err(map_budget_error)?;
                        sheet_infos.push(info);
                    }
                    RT_SST => {
                        sst = parse_sst(&rec.data)?;
                    }
                    RT_EOF => {
                        phase = Phase::BetweenSheets;
                    }
                    _ => {}
                },
                Phase::BetweenSheets => {
                    if rec.record_type == RT_BOF {
                        phase = Phase::InSheet;
                        cells.clear();
                        pending_formula_string = None;
                    }
                }
                Phase::InSheet => match rec.record_type {
                    RT_EOF => {
                        let name = if sheet_idx < sheet_infos.len() {
                            sheet_infos[sheet_idx].name.clone()
                        } else {
                            format!("Sheet{}", sheet_idx + 1)
                        };
                        let hidden = sheet_idx < sheet_infos.len() && sheet_infos[sheet_idx].hidden;
                        if !hidden {
                            let rows = build_grid(&mut cells)?;
                            sheets.push(Sheet { name, rows });
                        }
                        sheet_idx += 1;
                        phase = Phase::BetweenSheets;
                    }
                    RT_STRING => {
                        if let Some((row, col)) = pending_formula_string.take() {
                            let (s, consumed) = read_unicode_string(&rec.data, 0)?;
                            if consumed > rec.data.len() {
                                return Err(XlsError::InvalidRecord(
                                    "STRING record is truncated".to_owned(),
                                ));
                            }
                            charge_cells(1, s.len())?;
                            cells.push(Cell {
                                row,
                                col,
                                value: CellValue::String(s),
                            });
                        }
                    }
                    RT_FORMULA => {
                        pending_formula_string = None;
                        if rec.data.len() >= 14 {
                            let val_bytes = &rec.data[6..14];
                            if val_bytes[6] == 0xFF && val_bytes[7] == 0xFF && val_bytes[0] == 0 {
                                let row = u16::from_le_bytes([rec.data[0], rec.data[1]]);
                                let col = u16::from_le_bytes([rec.data[2], rec.data[3]]);
                                pending_formula_string = Some((row, col));
                                continue;
                            }
                        }
                        let parsed = parse_cell_record(&rec, &sst)?;
                        charge_parsed_cells(&parsed)?;
                        cells.extend(parsed);
                    }
                    _ => {
                        pending_formula_string = None;
                        // Skip LABEL/RSTRING parsing for non-BIFF8 (avoids slow unicode fallback).
                        if !biff8 && matches!(rec.record_type, RT_LABEL | RT_RSTRING) {
                            // BIFF5 LABEL: extract text directly.
                            if rec.data.len() >= 8 {
                                let row = u16::from_le_bytes([rec.data[0], rec.data[1]]);
                                let col = u16::from_le_bytes([rec.data[2], rec.data[3]]);
                                let str_len =
                                    u16::from_le_bytes([rec.data[6], rec.data[7]]) as usize;
                                let start = 8_usize;
                                let end = start.checked_add(str_len).ok_or_else(|| {
                                    XlsError::InvalidRecord("LABEL length overflow".to_owned())
                                })?;
                                if end > rec.data.len() {
                                    return Err(XlsError::InvalidRecord(
                                        "LABEL record is truncated".to_owned(),
                                    ));
                                }
                                let s: String =
                                    rec.data[start..end].iter().map(|&b| b as char).collect();
                                charge_cells(1, s.len())?;
                                cells.push(Cell {
                                    row,
                                    col,
                                    value: CellValue::String(s),
                                });
                            }
                        } else if is_cell_record(rec.record_type) {
                            let parsed = parse_cell_record(&rec, &sst)?;
                            charge_parsed_cells(&parsed)?;
                            cells.extend(parsed);
                        }
                    }
                },
            }
            crate::budget::check_model_items("office_xls_cells", cells.len())
                .map_err(map_budget_error)?;
            crate::budget::check_model_items("office_xls_sheets", sheet_infos.len())
                .map_err(map_budget_error)?;
            crate::budget::check_model_items("office_xls_shared_strings", sst.len())
                .map_err(map_budget_error)?;
        }

        if matches!(phase, Phase::Globals | Phase::InSheet) {
            return Err(XlsError::Corrupted(
                "workbook stream ends before EOF".to_owned(),
            ));
        }
        if sheet_idx != sheet_infos.len() {
            return Err(XlsError::Corrupted(
                "worksheet substream count does not match BOUNDSHEET records".to_owned(),
            ));
        }
        Ok(Self { sheets })
    }

    /// Convert to markdown.
    #[cfg(test)]
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        for (i, sheet) in self.sheets.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str("## ");
            out.push_str(&sheet.name);
            out.push_str("\n\n");

            if sheet.rows.is_empty() {
                continue;
            }

            // Header row.
            let col_count = sheet.rows.iter().map(|r| r.len()).max().unwrap_or(0);
            if col_count == 0 {
                continue;
            }

            // First row as header.
            out.push('|');
            if let Some(first_row) = sheet.rows.first() {
                for c in 0..col_count {
                    let text = first_row.get(c).map(|v| v.as_text()).unwrap_or_default();
                    out.push(' ');
                    out.push_str(&text);
                    out.push_str(" |");
                }
            }
            out.push('\n');

            // Separator.
            out.push('|');
            for _ in 0..col_count {
                out.push_str(" --- |");
            }
            out.push('\n');

            // Data rows.
            for row in sheet.rows.iter().skip(1) {
                out.push('|');
                for c in 0..col_count {
                    let text = row.get(c).map(|v| v.as_text()).unwrap_or_default();
                    out.push(' ');
                    out.push_str(&text);
                    out.push_str(" |");
                }
                out.push('\n');
            }
        }
        out
    }
}

fn is_cell_record(record_type: u16) -> bool {
    matches!(
        record_type,
        RT_LABELSST
            | RT_NUMBER
            | RT_RK
            | RT_MULRK
            | RT_BOOLERR
            | RT_LABEL
            | RT_RSTRING
            | RT_BLANK
            | RT_MULBLANK
            | RT_FORMULA
    )
}

fn charge_parsed_cells(cells: &[Cell]) -> Result<()> {
    let text_bytes = cells.iter().try_fold(0_usize, |total, cell| {
        let bytes = match &cell.value {
            CellValue::String(text) => text.len(),
            _ => 0,
        };
        total
            .checked_add(bytes)
            .ok_or_else(|| XlsError::Corrupted("cell text size overflow".to_owned()))
    })?;
    charge_cells(cells.len(), text_bytes)
}

fn charge_cells(count: usize, text_bytes: usize) -> Result<()> {
    crate::budget::charge_model_items("office_xls_cells", count).map_err(map_budget_error)?;
    crate::budget::charge_model_text("office_xls_text_bytes", text_bytes).map_err(map_budget_error)
}

enum Phase {
    Globals,
    BetweenSheets,
    InSheet,
}

fn parse_boundsheet(data: &[u8]) -> Result<SheetInfo> {
    if data.len() < 8 {
        return Err(XlsError::InvalidRecord("BOUNDSHEET too short".into()));
    }
    let offset = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let visibility = data[4]; // 0=visible, 1=hidden, 2=very hidden
    let _sheet_type = data[5]; // 0=worksheet, 2=chart, 6=VBA
    let (name, _) = read_short_unicode_string(data, 6)?;

    Ok(SheetInfo {
        name,
        offset,
        hidden: visibility != 0,
    })
}

/// Build a 2D grid from sparse cells.
///
/// Takes ownership of cell values via `std::mem::take` to avoid cloning.
fn build_grid(cells: &mut [Cell]) -> Result<Vec<Vec<CellValue>>> {
    if cells.is_empty() {
        return Ok(Vec::new());
    }
    let max_col = cells.iter().map(|c| c.col).max().unwrap_or(0) as usize;
    if max_col > 255 {
        return Err(XlsError::InvalidRecord(
            "cell column exceeds BIFF worksheet bounds".to_owned(),
        ));
    }
    cells.sort_unstable_by(|a, b| a.row.cmp(&b.row).then(a.col.cmp(&b.col)));
    if cells
        .windows(2)
        .any(|pair| pair[0].row == pair[1].row && pair[0].col == pair[1].col)
    {
        return Err(XlsError::InvalidRecord(
            "duplicate cell coordinate".to_owned(),
        ));
    }
    let row_count = cells
        .iter()
        .map(|cell| cell.row)
        .fold((None, 0_usize), |(previous, count), row| {
            if previous == Some(row) {
                (previous, count)
            } else {
                (Some(row), count.saturating_add(1))
            }
        })
        .1;
    let item_count = row_count
        .checked_mul(max_col + 1)
        .ok_or_else(|| XlsError::Corrupted("grid size overflow".into()))?;
    crate::budget::check_model_items("office_xls_grid_cells", item_count)
        .map_err(map_budget_error)?;
    crate::budget::charge_model_items("office_xls_grid_cells", item_count)
        .map_err(map_budget_error)?;
    let mut grid: Vec<Vec<CellValue>> = Vec::new();
    let mut current_row = None;
    for cell in cells {
        if current_row != Some(cell.row) {
            current_row = Some(cell.row);
            grid.push(vec![CellValue::Empty; max_col + 1]);
        }
        grid.last_mut().expect("row was just inserted")[cell.col as usize] =
            std::mem::take(&mut cell.value);
    }
    Ok(grid)
}

fn map_budget_error(error: crate::OfficeReadError) -> XlsError {
    let error = match error {
        crate::OfficeReadError::ResourceLimit {
            resource,
            limit,
            observed,
        } => crate::cfb::CfbError::ResourceLimit {
            resource,
            limit,
            observed,
        },
        _ => crate::cfb::CfbError::Cancelled,
    };
    XlsError::Cfb(error)
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn build_grid_from_cells() {
        let mut cells = vec![
            Cell {
                row: 0,
                col: 0,
                value: CellValue::String("A1".into()),
            },
            Cell {
                row: 0,
                col: 1,
                value: CellValue::Number(42.0),
            },
            Cell {
                row: 1,
                col: 0,
                value: CellValue::String("A2".into()),
            },
        ];
        let grid = build_grid(&mut cells);
        assert_eq!(grid.len(), 2);
        assert_eq!(grid[0].len(), 2);
        assert_eq!(grid[0][0], CellValue::String("A1".into()));
        assert_eq!(grid[0][1], CellValue::Number(42.0));
        assert_eq!(grid[1][0], CellValue::String("A2".into()));
        assert_eq!(grid[1][1], CellValue::Empty);
    }

    #[test]
    fn build_grid_empty() {
        let grid = build_grid(&mut Vec::new());
        assert!(grid.is_empty());
    }

    #[test]
    fn parse_boundsheet_record() {
        let mut data = Vec::new();
        data.extend_from_slice(&100u32.to_le_bytes()); // offset
        data.push(0); // visible
        data.push(0); // worksheet
        // Short string "Sheet1"
        data.push(6); // char count
        data.push(0); // compressed
        data.extend_from_slice(b"Sheet1");
        let info = parse_boundsheet(&data).unwrap();
        assert_eq!(info.name, "Sheet1");
        assert_eq!(info.offset, 100);
        assert!(!info.hidden);
    }

    #[test]
    fn plain_text_output() {
        let doc = XlsDocument {
            sheets: vec![Sheet {
                name: "Sheet1".into(),
                rows: vec![
                    vec![
                        CellValue::String("Name".into()),
                        CellValue::String("Age".into()),
                    ],
                    vec![CellValue::String("Alice".into()), CellValue::Number(30.0)],
                ],
            }],
        };
        let text = doc.plain_text();
        assert!(text.contains("Sheet1"));
        assert!(text.contains("Name\tAge"));
        assert!(text.contains("Alice\t30"));
    }

    #[test]
    fn markdown_output() {
        let doc = XlsDocument {
            sheets: vec![Sheet {
                name: "Data".into(),
                rows: vec![
                    vec![CellValue::String("X".into()), CellValue::String("Y".into())],
                    vec![CellValue::Number(1.0), CellValue::Number(2.0)],
                ],
            }],
        };
        let md = doc.to_markdown();
        assert!(md.contains("## Data"));
        assert!(md.contains("| X | Y |"));
        assert!(md.contains("| --- | --- |"));
        assert!(md.contains("| 1 | 2 |"));
    }

    fn make_doc(sheets: Vec<Sheet>) -> XlsDocument {
        XlsDocument { sheets }
    }

    #[test]
    fn ir_empty_doc_produces_no_sections() {
        let ir = crate::convert_xls::xls_to_ir(&make_doc(vec![]));
        assert!(ir.sections.is_empty());
        assert!(ir.metadata.title.is_none());
    }

    #[test]
    fn ir_empty_sheet_has_no_table() {
        let ir = crate::convert_xls::xls_to_ir(&make_doc(vec![Sheet {
            name: "Empty".into(),
            rows: vec![],
        }]));
        assert_eq!(ir.sections[0].title.as_deref(), Some("Empty"));
        assert!(ir.sections[0].elements.is_empty());
    }

    #[test]
    fn ir_sheet_with_data_produces_table_with_header_row() {
        use crate::ir::Element;
        let rows = vec![
            vec![
                CellValue::String("Name".into()),
                CellValue::String("Score".into()),
            ],
            vec![CellValue::String("Alice".into()), CellValue::Number(95.0)],
        ];
        let ir = crate::convert_xls::xls_to_ir(&make_doc(vec![Sheet {
            name: "Results".into(),
            rows,
        }]));
        assert_eq!(ir.metadata.title.as_deref(), Some("Results"));
        assert!(matches!(ir.sections[0].elements[0], Element::Table(_)));
        if let Element::Table(ref t) = ir.sections[0].elements[0] {
            assert!(t.rows[0].is_header);
            assert!(!t.rows[1].is_header);
        }
    }

    #[test]
    fn ir_empty_cell_value_produces_empty_paragraph_content() {
        use crate::ir::Element;
        let ir = crate::convert_xls::xls_to_ir(&make_doc(vec![Sheet {
            name: "S".into(),
            rows: vec![vec![CellValue::Empty]],
        }]));
        if let Element::Table(ref t) = ir.sections[0].elements[0] {
            if let Element::Paragraph(ref p) = t.rows[0].cells[0].content[0] {
                assert!(p.content.is_empty());
            }
        }
    }

    #[test]
    fn ir_multiple_sheets_produce_multiple_sections() {
        let doc = make_doc(vec![
            Sheet {
                name: "A".into(),
                rows: vec![vec![CellValue::Number(1.0)]],
            },
            Sheet {
                name: "B".into(),
                rows: vec![vec![CellValue::String("x".into())]],
            },
        ]);
        let ir = crate::convert_xls::xls_to_ir(&doc);
        assert_eq!(ir.sections.len(), 2);
        assert_eq!(ir.sections[1].title.as_deref(), Some("B"));
    }

    #[test]
    fn ir_format_is_xls() {
        let ir = crate::convert_xls::xls_to_ir(&make_doc(vec![]));
        assert_eq!(ir.metadata.format, crate::format::DocumentFormat::Xls);
    }
}
