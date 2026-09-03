use super::document::BlockElement;

#[derive(Debug, Clone, Default)]
pub struct SectionProperties {
    pub header_refs: Vec<HeaderFooterRef>,
    pub footer_refs: Vec<HeaderFooterRef>,
}

#[derive(Debug, Clone)]
pub struct HeaderFooterRef {
    pub relationship_id: String,
}

#[derive(Debug, Clone)]
pub struct HeaderFooter {
    pub content: Vec<BlockElement>,
    pub is_header: bool,
}
