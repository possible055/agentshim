use super::paragraph::Paragraph;
use super::table::Table;

/// The document body, containing all block-level elements.
#[derive(Debug, Clone, Default)]
pub struct Body {
    /// Ordered list of block elements (paragraphs and tables).
    pub elements: Vec<BlockElement>,
}

/// A block-level element in the document body (or in a table cell).
#[allow(
    clippy::large_enum_variant,
    reason = "boxing Paragraph would force a heap allocation per paragraph on the hot parse path"
)]
#[derive(Debug, Clone)]
pub enum BlockElement {
    /// A paragraph (`w:p`).
    Paragraph(Paragraph),
    /// A table (`w:tbl`).
    Table(Table),
}
