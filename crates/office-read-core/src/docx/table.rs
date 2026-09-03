use super::document::BlockElement;

/// A table element (`w:tbl`).
#[derive(Debug, Clone)]
pub struct Table {
    /// Rows in the table.
    pub rows: Vec<TableRow>,
}

/// A table row (`w:tr`).
#[derive(Debug, Clone)]
pub struct TableRow {
    /// Cells in this row.
    pub cells: Vec<TableCell>,
}

/// A table cell (`w:tc`).
#[derive(Debug, Clone)]
pub struct TableCell {
    /// Block content within the cell.
    pub content: Vec<BlockElement>,
}
