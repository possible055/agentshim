use super::XlsxDocument;
use super::cell::{Cell, CellValue};
use super::date;
use super::numfmt;
use super::worksheet::{HyperlinkTarget, Row, Worksheet};

impl XlsxDocument {
    pub(crate) fn sheet_has_markdown(&self, index: usize) -> bool {
        self.worksheets.get(index).is_some_and(|worksheet| {
            !worksheet.rows.is_empty() && compute_column_count(&worksheet.rows) > 0
        })
    }

    pub(crate) fn sheet_uses_prose_markdown(&self, index: usize) -> bool {
        self.worksheets.get(index).is_some_and(|worksheet| {
            compute_column_count(&worksheet.rows) == 1
                && worksheet.rows.iter().any(|row| {
                    row.cells
                        .first()
                        .is_some_and(|cell| self.format_cell_value(cell).chars().count() > 20)
                })
        })
    }

    pub(crate) fn sheet_markdown_start(&self, index: usize, prose: bool) -> Option<String> {
        let worksheet = self.worksheets.get(index)?;
        if prose {
            return Some(format!("## {}", worksheet.name));
        }
        let column_count = compute_column_count(&worksheet.rows);
        let header = worksheet.rows.first()?;
        let mut markdown = format!("## {}\n\n|", worksheet.name);
        for column in 0..column_count {
            markdown.push(' ');
            if let Some(cell) = cell_at(header, column) {
                self.write_cell_markdown(worksheet, cell, &mut markdown);
            }
            markdown.push_str(" |");
        }
        markdown.push_str("\n|");
        for _ in 0..column_count {
            markdown.push_str(" --- |");
        }
        Some(markdown)
    }

    pub(crate) fn sheet_markdown_row(
        &self,
        sheet: usize,
        row: usize,
        prose: bool,
    ) -> Option<String> {
        let worksheet = self.worksheets.get(sheet)?;
        let row = worksheet.rows.get(row)?;
        if prose {
            let text = row
                .cells
                .first()
                .map(|cell| self.format_cell_markdown(worksheet, cell))?;
            let text = text.trim();
            return (!text.is_empty()).then(|| format!("\n\n{text}"));
        }
        let column_count = compute_column_count(&worksheet.rows);
        let mut markdown = String::from("\n|");
        for column in 0..column_count {
            markdown.push(' ');
            if let Some(cell) = cell_at(row, column) {
                self.write_cell_markdown(worksheet, cell, &mut markdown);
            }
            markdown.push_str(" |");
        }
        Some(markdown)
    }

    /// Convert to markdown (pipe-delimited tables).
    #[cfg(test)]
    pub fn to_markdown(&self) -> String {
        let mut parts = Vec::new();
        for i in 0..self.worksheets.len() {
            if let Some(md) = self.sheet_to_markdown(i) {
                if !md.is_empty() {
                    parts.push(md);
                }
            }
            if let Some(worksheet) = self.worksheets.get(i) {
                for alt_text in &worksheet.picture_alt_text {
                    parts.push(format!("> Image: {alt_text}"));
                }
            }
        }
        // Charts: emit each chart's extracted text under a "## Chart N" heading
        // so its words appear in markdown / search / PDF without needing a
        // graphical chart renderer.
        for (i, text) in self.chart_text.iter().enumerate() {
            if !text.trim().is_empty() {
                parts.push(format!("## Chart {}\n\n{}", i + 1, text));
            }
        }
        parts.join("\n\n")
    }

    /// Convert specific sheet to markdown.
    #[cfg(test)]
    pub fn sheet_to_markdown(&self, sheet_index: usize) -> Option<String> {
        let ws = self.worksheets.get(sheet_index)?;
        if ws.rows.is_empty() {
            return Some(String::new());
        }

        let col_count = compute_column_count(&ws.rows);
        if col_count == 0 {
            return Some(String::new());
        }

        // If the sheet is effectively single-column with prose-length cells
        // (notes, single-column reports), emit each cell as its own paragraph
        // instead of wrapping every line in a 1-column GFM table. The table
        // form looks awful when rendered (tall, narrow, hard to read) and
        // round-trips badly through markdown→IR→office.
        if col_count == 1
            && ws.rows.iter().any(|r| {
                r.cells
                    .first()
                    .map(|c| self.format_cell_value(c).chars().count() > 20)
                    .unwrap_or(false)
            })
        {
            let mut out = String::new();
            out.push_str(&format!("## {}\n\n", ws.name));
            for row in &ws.rows {
                if crate::budget::is_cancelled() {
                    break;
                }
                if let Some(cell) = row.cells.first() {
                    let text = self.format_cell_value(cell);
                    if !text.trim().is_empty() {
                        out.push_str(text.trim());
                        out.push_str("\n\n");
                        if !crate::budget::markdown_within_limit(out.len()) {
                            break;
                        }
                    }
                }
            }
            return Some(out.trim_end().to_string());
        }

        let mut lines = Vec::new();

        // Sheet name as heading
        lines.push(format!("## {}", ws.name));
        lines.push(String::new());

        // First row as header
        let header_row = &ws.rows[0];
        let header_cells: Vec<String> = (0..col_count)
            .map(|i| {
                header_row
                    .cells
                    .get(i)
                    .map(|c| self.format_cell_value(c))
                    .unwrap_or_default()
            })
            .collect();
        lines.push(format!("| {} |", header_cells.join(" | ")));

        // Separator row
        let sep: Vec<&str> = vec!["---"; col_count];
        lines.push(format!("| {} |", sep.join(" | ")));

        // Data rows
        for row in ws.rows.iter().skip(1) {
            if crate::budget::is_cancelled() {
                break;
            }
            let cells: Vec<String> = (0..col_count)
                .map(|i| {
                    row.cells
                        .get(i)
                        .map(|c| self.format_cell_value(c))
                        .unwrap_or_default()
                })
                .collect();
            lines.push(format!("| {} |", cells.join(" | ")));
            let observed = lines.iter().map(|line| line.len() + 1).sum();
            if !crate::budget::markdown_within_limit(observed) {
                break;
            }
        }

        Some(lines.join("\n"))
    }

    /// Format a cell value to a display string, applying date detection.
    pub fn format_cell_value(&self, cell: &Cell) -> String {
        let mut buf = String::new();
        self.write_cell_value(cell, &mut buf);
        buf
    }

    /// Write a cell value directly to a buffer (avoids allocation for shared strings).
    pub fn write_cell_value(&self, cell: &Cell, buf: &mut String) {
        match &cell.value {
            CellValue::Empty => {}
            CellValue::Number(n) => {
                if date::is_date_cell(cell.style_index, self.styles.as_ref()) {
                    if let Some(dt) = date::DateTimeValue::from_serial(*n, self.workbook.date1904) {
                        buf.push_str(&dt.to_iso_string());
                        return;
                    }
                }
                if let Some(idx) = cell.style_index {
                    if let Some(styles) = self.styles.as_ref() {
                        if let Some(fmt_id) = styles.number_format_id_for(idx) {
                            if fmt_id != 0 {
                                let fmt_str = styles.number_format_for(idx);
                                let formatted = numfmt::apply_format(*n, fmt_id, fmt_str);
                                buf.push_str(&formatted);
                                return;
                            }
                        }
                    }
                }
                write_number(*n, buf);
            }
            CellValue::String(s) => buf.push_str(s),
            CellValue::SharedString(idx) => {
                let s = self.shared_strings.get(*idx).unwrap_or("");
                buf.push_str(s);
            }
            CellValue::Boolean(b) => buf.push_str(if *b { "TRUE" } else { "FALSE" }),
            CellValue::Error(e) => buf.push_str(e),
        }
    }

    fn format_cell_markdown(&self, worksheet: &Worksheet, cell: &Cell) -> String {
        let mut markdown = String::new();
        self.write_cell_markdown(worksheet, cell, &mut markdown);
        markdown
    }

    fn write_cell_markdown(&self, worksheet: &Worksheet, cell: &Cell, out: &mut String) {
        let hyperlink = worksheet
            .hyperlinks
            .iter()
            .find(|hyperlink| hyperlink.cell_ref == cell.reference.to_string());
        if let Some(hyperlink) = hyperlink {
            let mut label = String::new();
            self.write_cell_value(cell, &mut label);
            let target = match &hyperlink.target {
                HyperlinkTarget::External(target) => target.as_str(),
                HyperlinkTarget::Internal(target) => {
                    out.push('[');
                    out.push_str(&label);
                    out.push_str("](#");
                    out.push_str(target);
                    out.push(')');
                    return;
                }
            };
            out.push('[');
            out.push_str(&label);
            out.push_str("](");
            out.push_str(target);
            out.push(')');
        } else {
            self.write_cell_value(cell, out);
        }
    }
}

/// Write a formatted number directly to a buffer.
fn write_number(n: f64, buf: &mut String) {
    use std::fmt::Write;
    if n == n.trunc() && n.abs() < 1e15 {
        write!(buf, "{}", n as i64).ok();
    } else {
        write!(buf, "{n}").ok();
    }
}

/// Compute the maximum number of columns across all rows.
fn compute_column_count(rows: &[Row]) -> usize {
    rows.iter()
        .flat_map(|row| row.cells.iter())
        .map(|cell| cell.reference.col as usize + 1)
        .max()
        .unwrap_or(0)
}

fn cell_at(row: &Row, column: usize) -> Option<&Cell> {
    row.cells
        .binary_search_by_key(&(column as u32), |cell| cell.reference.col)
        .ok()
        .and_then(|index| row.cells.get(index))
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn csv_escape_plain() {
        assert_eq!(csv_escape("hello"), "hello");
    }

    #[test]
    fn csv_escape_with_comma() {
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
    }

    #[test]
    fn csv_escape_with_quotes() {
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn csv_escape_with_newline() {
        assert_eq!(csv_escape("line1\nline2"), "\"line1\nline2\"");
    }

    fn fmt_num(n: f64) -> String {
        let mut buf = String::new();
        write_number(n, &mut buf);
        buf
    }

    #[test]
    fn format_number_integer() {
        assert_eq!(fmt_num(42.0), "42");
        assert_eq!(fmt_num(0.0), "0");
        assert_eq!(fmt_num(-10.0), "-10");
    }

    #[test]
    fn format_number_float() {
        assert_eq!(fmt_num(3.15), "3.15");
        assert_eq!(fmt_num(0.5), "0.5");
    }
}
