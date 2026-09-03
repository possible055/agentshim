/// Information about a drawing/image reference within a run.
#[derive(Debug, Clone)]
pub struct DrawingInfo {
    /// Relationship ID pointing to the image part.
    pub relationship_id: String,
    /// Alt-text description from `wp:docPr/@descr`.
    pub description: Option<String>,
}
