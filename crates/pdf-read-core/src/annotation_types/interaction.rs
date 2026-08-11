/// File attachment annotation icon per PDF spec Section 12.5.6.15.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileAttachmentIcon {
    /// Graph/push pin icon
    GraphPushPin,
    /// Paperclip tag icon (default)
    #[default]
    PaperclipTag,
    /// Push pin icon
    PushPin,
}

impl FileAttachmentIcon {
    /// Get PDF name.
    pub fn pdf_name(&self) -> &'static str {
        match self {
            Self::GraphPushPin => "GraphPushPin",
            Self::PaperclipTag => "PaperclipTag",
            Self::PushPin => "PushPin",
        }
    }

    /// Parse from PDF name.
    pub fn from_pdf_name(name: &str) -> Self {
        match name {
            "GraphPushPin" => Self::GraphPushPin,
            "PushPin" => Self::PushPin,
            _ => Self::PaperclipTag,
        }
    }
}

/// Reply type for annotation replies per PDF spec Table 170.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplyType {
    /// Reply annotation
    #[default]
    Reply,
    /// Group annotation
    Group,
}

impl ReplyType {
    /// Get PDF name.
    pub fn pdf_name(&self) -> &'static str {
        match self {
            Self::Reply => "R",
            Self::Group => "Group",
        }
    }

    /// Parse from PDF name.
    pub fn from_pdf_name(name: &str) -> Self {
        match name {
            "Group" => Self::Group,
            _ => Self::Reply,
        }
    }
}

/// Highlight mode for link annotations per PDF spec Table 173.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HighlightMode {
    /// No highlighting (N)
    None,
    /// Invert the contents (I) - default
    #[default]
    Invert,
    /// Invert the border (O)
    Outline,
    /// Push effect (P)
    Push,
}

impl HighlightMode {
    /// Get PDF name.
    pub fn pdf_name(&self) -> &'static str {
        match self {
            Self::None => "N",
            Self::Invert => "I",
            Self::Outline => "O",
            Self::Push => "P",
        }
    }

    /// Parse from PDF name.
    pub fn from_pdf_name(name: &str) -> Self {
        match name {
            "N" => Self::None,
            "O" => Self::Outline,
            "P" => Self::Push,
            _ => Self::Invert,
        }
    }
}

/// Widget field type for form fields.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum WidgetFieldType {
    /// Text input field
    #[default]
    Text,
    /// Checkbox
    Checkbox {
        /// Whether the checkbox is checked
        checked: bool,
    },
    /// Radio button
    Radio {
        /// Selected option value
        selected: Option<String>,
    },
    /// Push button
    Button,
    /// Choice field (dropdown or list)
    Choice {
        /// Available options
        options: Vec<String>,
        /// Selected option(s)
        selected: Option<String>,
    },
    /// Signature field
    Signature,
    /// Unknown field type
    Unknown,
}

/// A quad point specification (8 numbers defining a quadrilateral).
///
/// Points are specified as [x1, y1, x2, y2, x3, y3, x4, y4] in counterclockwise order.
/// The bottom edge is from (x1, y1) to (x2, y2).
pub type QuadPoint = [f64; 8];

/// Helper functions for quad points.
pub mod quad_points {
    use super::QuadPoint;
    use crate::geometry::Rect;

    /// Create a quad point from a rectangle.
    pub fn from_rect(rect: &Rect) -> QuadPoint {
        let x1 = rect.x as f64;
        let y1 = rect.y as f64;
        let x2 = (rect.x + rect.width) as f64;
        let y2 = rect.y as f64;
        let x3 = (rect.x + rect.width) as f64;
        let y3 = (rect.y + rect.height) as f64;
        let x4 = rect.x as f64;
        let y4 = (rect.y + rect.height) as f64;

        [x1, y1, x2, y2, x3, y3, x4, y4]
    }

    /// Get the bounding rectangle of a quad point.
    pub fn bounding_rect(quad: &QuadPoint) -> Rect {
        let min_x = quad[0].min(quad[2]).min(quad[4]).min(quad[6]) as f32;
        let max_x = quad[0].max(quad[2]).max(quad[4]).max(quad[6]) as f32;
        let min_y = quad[1].min(quad[3]).min(quad[5]).min(quad[7]) as f32;
        let max_y = quad[1].max(quad[3]).max(quad[5]).max(quad[7]) as f32;

        Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
    }

    /// Parse quad points from a flat array of numbers.
    pub fn parse(arr: &[f64]) -> Vec<QuadPoint> {
        arr.chunks_exact(8)
            .map(|chunk| {
                [
                    chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
                ]
            })
            .collect()
    }

    /// Flatten quad points to a single array.
    pub fn flatten(quads: &[QuadPoint]) -> Vec<f64> {
        quads.iter().flat_map(|q| q.iter().copied()).collect()
    }
}
