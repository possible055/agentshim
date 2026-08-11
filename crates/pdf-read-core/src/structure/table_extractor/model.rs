use super::*;

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

impl Table {
    /// Create a new extracted table
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            has_header: false,
            col_count: 0,
            bbox: None,
        }
    }

    /// Check whether this table looks like a real data grid as opposed to
    /// spurious spatial output (form layouts, label-colon-value pairs).
    ///
    /// A real grid (per #457 Step 4) has:
    /// - ≥2 rows
    /// - ≥2 columns
    /// - Consistent column population: at least 50% of rows must have
    ///   at least 2 non-empty cells. Filters out the common
    ///   form-as-table false positive where rows look like
    ///   `| Single label, all other slots empty | | | |`.
    pub fn is_real_grid(&self) -> bool {
        if self.col_count < 2 || self.rows.len() < 2 {
            return false;
        }
        let rows_with_two_or_more_filled_cells = self
            .rows
            .iter()
            .filter(|r| r.cells.iter().filter(|c| !c.text.trim().is_empty()).count() >= 2)
            .count();
        let ratio = rows_with_two_or_more_filled_cells as f32 / self.rows.len() as f32;

        // Wide tables (≥ 8 columns) are high-risk false positives: prose sentences
        // can be split into many single-phrase cells by decorative rule lines.
        // Real wide data tables have most rows densely filled (≥ 60% of columns);
        // prose-split false tables have highly variable row fill counts (some rows
        // have 1-2 filled cells, others have 10+), so the fraction of "dense" rows
        // is well below 70%.
        //
        // Exception: a consolidated multi-row table (issue 486) can contain a mix
        // of dense data rows and sparse header / multi-row-label rows.  The sparse
        // rows are legitimate table content (column headers split across multiple
        // visual rows, lane-count labels that only appear on the first row of a
        // sub-group), so a strict dense-row ratio rejects real tables.  Accept the
        // table if it has BOTH enough dense rows in absolute terms (≥ half the
        // column count) AND a meaningful dense-row ratio (≥ 40 %).
        if self.col_count >= 8 {
            let min_dense = ((self.col_count as f32 * 0.6) as usize).max(2);
            let dense_rows = self
                .rows
                .iter()
                .filter(|r| {
                    r.cells.iter().filter(|c| !c.text.trim().is_empty()).count() >= min_dense
                })
                .count();
            let dense_row_ratio = dense_rows as f32 / self.rows.len() as f32;
            if self.rows.len() >= 3 && ratio >= 0.7 && dense_row_ratio >= 0.70 {
                return true;
            }
            // Consolidated-table path: accept tables with many absolutely-dense
            // rows alongside sparse header/label rows (issue 486).
            let min_absolute_dense = (self.col_count / 2).max(3);
            return dense_rows >= min_absolute_dense && dense_row_ratio >= 0.40;
        }

        ratio >= 0.5
    }

    /// Render the table as clean, space-padded plain text.
    pub fn render_text(&self) -> String {
        let col_count = self.col_count;
        if col_count == 0 || self.rows.is_empty() {
            return String::new();
        }

        // Calculate column widths from cell content
        let mut col_widths = vec![0usize; col_count];
        for row in &self.rows {
            let mut col_idx = 0;
            for cell in &row.cells {
                if cell.colspan == 1 && col_idx < col_count {
                    let w = cell.text.trim().chars().count();
                    col_widths[col_idx] = col_widths[col_idx].max(w);
                }
                col_idx += cell.colspan as usize;
            }
        }

        // Ensure minimum width of 2 per column
        for w in &mut col_widths {
            if *w < 2 {
                *w = 2;
            }
        }

        // Trim trailing empty columns (no non-empty cell contributes content
        // to that column, including cells with colspan > 1 that cover it).
        let effective_cols = {
            let mut eff = col_widths.len();
            while eff > 0 {
                let col = eff - 1;
                let all_empty = self.rows.iter().all(|row| {
                    let mut ci = 0;
                    for cell in &row.cells {
                        let span = cell.colspan as usize;
                        let covers_col = ci <= col && col < ci + span;
                        if covers_col {
                            return cell.text.trim().is_empty();
                        }
                        ci += span;
                    }
                    true
                });
                if all_empty {
                    eff -= 1;
                } else {
                    break;
                }
            }
            eff
        };
        if effective_cols == 0 {
            return String::new();
        }
        let col_widths = &col_widths[..effective_cols];

        // Detect right-aligned columns (all non-empty cells look like numbers/currency)
        let is_right_aligned: Vec<bool> = (0..effective_cols)
            .map(|c| {
                let mut has_content = false;
                for row in &self.rows {
                    let mut ci = 0;
                    for cell in &row.cells {
                        if ci == c && cell.colspan == 1 {
                            let t = cell.text.trim();
                            if !t.is_empty() {
                                has_content = true;
                                // Check if it looks like a number or currency value
                                let stripped: String = t
                                    .chars()
                                    .filter(|ch| {
                                        !matches!(
                                            ch,
                                            '$' | '€'
                                                | '£'
                                                | ','
                                                | ' '
                                                | '%'
                                                | '+'
                                                | '-'
                                                | '('
                                                | ')'
                                        )
                                    })
                                    .collect();
                                if stripped.is_empty() || stripped.parse::<f64>().is_err() {
                                    return false;
                                }
                            }
                        }
                        ci += cell.colspan as usize;
                    }
                }
                has_content
            })
            .collect();

        let mut output = String::new();

        for row in &self.rows {
            let mut col_idx = 0;
            let mut cells_text = Vec::new();
            for cell in &row.cells {
                let text = cell.text.trim();
                if col_idx < effective_cols {
                    // For colspan > 1, calculate merged width
                    let width = if cell.colspan > 1 {
                        let end = (col_idx + cell.colspan as usize).min(effective_cols);
                        let base: usize = col_widths[col_idx..end].iter().sum();
                        // Add 2 spaces per gap between merged columns
                        base + (end - col_idx).saturating_sub(1) * 2
                    } else {
                        col_widths[col_idx]
                    };
                    let formatted = if cell.colspan == 1 && is_right_aligned[col_idx] {
                        format!("{:>width$}", text, width = width)
                    } else {
                        format!("{:<width$}", text, width = width)
                    };
                    cells_text.push(formatted);
                } else {
                    cells_text.push(text.to_string());
                }
                col_idx += cell.colspan as usize;
            }
            output.push_str(cells_text.join("  ").trim_end());
            output.push('\n');
        }

        output
    }

    /// Add a row to the table
    pub fn add_row(&mut self, row: TableRow) {
        if self.col_count == 0 && !row.cells.is_empty() {
            self.col_count = row.cells.len();
        }
        self.rows.push(row);
    }

    /// Check if table is empty
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl TableRow {
    /// Create a new table row
    pub fn new(is_header: bool) -> Self {
        Self {
            cells: Vec::new(),
            is_header,
        }
    }

    /// Check if any cell in this row has a colspan > 1 (v0.3.16).
    pub fn has_colspan(&self) -> bool {
        self.cells.iter().any(|c| c.colspan > 1)
    }

    /// Add a cell to the row
    pub fn add_cell(&mut self, cell: TableCell) {
        self.cells.push(cell);
    }
}

impl TableCell {
    /// Create a new table cell
    pub fn new(text: String, is_header: bool) -> Self {
        Self {
            text,
            spans: Vec::new(),
            colspan: 1,
            rowspan: 1,
            mcids: Vec::new(),
            bbox: None,
            is_header,
        }
    }

    /// Set colspan
    pub fn with_colspan(mut self, colspan: u32) -> Self {
        self.colspan = colspan;
        self
    }

    /// Set rowspan
    pub fn with_rowspan(mut self, rowspan: u32) -> Self {
        self.rowspan = rowspan;
        self
    }

    /// Add an MCID
    pub fn add_mcid(&mut self, mcid: u32) {
        self.mcids.push(mcid);
    }
}
