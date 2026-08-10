use super::*;

impl PdfDocument {
    /// Document Info dictionary `/Producer` (decoded, trimmed), if present
    /// and non-empty. A weak document-level prior for the scanner-vs-
    /// authoring heuristic (#517 case P) — never decisive.
    #[must_use]
    pub fn document_producer(&self) -> Option<String> {
        self.document_info_string("Producer")
    }

    /// Document Info dictionary `/Creator` (decoded, trimmed), if present
    /// and non-empty. See [`document_producer`](Self::document_producer).
    #[must_use]
    pub fn document_creator(&self) -> Option<String> {
        self.document_info_string("Creator")
    }

    fn document_info_string(&self, key: &str) -> Option<String> {
        let info_raw = self.trailer.as_dict()?.get("Info")?;
        let info = self.resolve_obj_ref(info_raw);
        let val_raw = info.as_dict()?.get(key)?.clone();
        let val = self.resolve_obj_ref(&val_raw);
        let s = Self::decode_pdf_text_string(val.as_string()?);
        let trimmed = s.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// Axis-aligned intersection area of a [`Rect`](crate::geometry::Rect)
    /// with the page box `(x0, y0, x1, y1)`.
    fn rect_isect_area(r: &crate::geometry::Rect, x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
        let (rx1, ry1) = (r.x + r.width, r.y + r.height);
        let ix = (rx1.min(x1) - r.x.max(x0)).max(0.0);
        let iy = (ry1.min(y1) - r.y.max(y0)).max(0.0);
        ix * iy
    }
}
