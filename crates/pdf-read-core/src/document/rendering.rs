use super::*;

impl PdfDocument {
    /// Get information about a page, including its dimensions.
    ///
    /// This is useful for rendering and layout calculations.
    #[cfg(feature = "rendering")]
    pub fn get_page_info(&self, page_index: usize) -> Result<PageInfo> {
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        // Helper to extract f32 from Integer or Real
        fn obj_to_f32(obj: &Object) -> Option<f32> {
            match obj {
                Object::Integer(i) => Some(*i as f32),
                Object::Real(r) => Some(*r as f32),
                _ => None,
            }
        }

        // Get MediaBox (required, may be inherited).
        // PDF spec §7.3.10: any value may be a direct or indirect reference —
        // including each individual array element (pdf.js issue7872 stores
        // `/MediaBox [4 0 R 5 0 R 6 0 R 7 0 R]`). Resolve every element,
        // otherwise an unresolved Reference reads as None and silently
        // falls back to the Letter-size default instead of the true bounds.
        let media_box = page_dict
            .get("MediaBox")
            .map(|o| self.resolve_obj_ref(o))
            .as_ref()
            .and_then(|o| o.as_array().map(|a| a.to_owned()))
            .map(|arr| {
                let r: Vec<Object> = arr.iter().map(|o| self.resolve_obj_ref(o)).collect();
                let x0 = r.first().and_then(obj_to_f32).unwrap_or(0.0);
                let y0 = r.get(1).and_then(obj_to_f32).unwrap_or(0.0);
                let x1 = r.get(2).and_then(obj_to_f32).unwrap_or(612.0);
                let y1 = r.get(3).and_then(obj_to_f32).unwrap_or(792.0);
                crate::geometry::Rect::from_points(x0, y0, x1, y1)
            })
            .unwrap_or(crate::geometry::Rect::from_points(
                0.0, 0.0, 612.0, 792.0, // Letter size default
            ));

        // Get CropBox (optional, falls back to MediaBox).
        // PDF spec §7.3.10: any value may be a direct or indirect reference.
        let crop_box = page_dict
            .get("CropBox")
            .map(|o| self.resolve_obj_ref(o))
            .as_ref()
            .and_then(|o| o.as_array().map(|a| a.to_owned()))
            .map(|arr| {
                let r: Vec<Object> = arr.iter().map(|o| self.resolve_obj_ref(o)).collect();
                let x0 = r.first().and_then(obj_to_f32).unwrap_or(0.0);
                let y0 = r.get(1).and_then(obj_to_f32).unwrap_or(0.0);
                let x1 = r.get(2).and_then(obj_to_f32).unwrap_or(612.0);
                let y1 = r.get(3).and_then(obj_to_f32).unwrap_or(792.0);
                crate::geometry::Rect::from_points(x0, y0, x1, y1)
            });

        // Get rotation (optional, default 0).
        // PDF spec Section 7.3.10: Rotate may also be an indirect reference.
        // Some producers emit it as a real number (e.g. /Rotate 90.0), which
        // the lexer parses as Object::Real - accept both, mirroring
        // get_page_rotation's Integer/Real handling (~line 4171 above).
        let rotation = page_dict
            .get("Rotate")
            .map(|o| self.resolve_obj_ref(o))
            .as_ref()
            .and_then(|o| match o {
                Object::Integer(i) => Some(*i as i32),
                Object::Real(r) => Some(*r as i32),
                _ => None,
            })
            .unwrap_or(0);

        Ok(PageInfo {
            media_box,
            crop_box,
            rotation,
        })
    }

    /// Get the resources dictionary for a page.
    ///
    /// Resources contain fonts, images, patterns, and other objects
    /// used when rendering the page.
    #[cfg(feature = "rendering")]
    pub fn get_page_resources(&self, page_index: usize) -> Result<Object> {
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        // Get Resources (required, may be inherited)
        let resources = page_dict
            .get("Resources")
            .cloned()
            .unwrap_or(Object::Dictionary(std::collections::HashMap::new()));

        // If it's a reference, resolve it
        if let Some(ref_val) = resources.as_reference() {
            self.load_object(ref_val)
        } else {
            Ok(resources)
        }
    }

    /// Resolve an object reference.
    ///
    /// This is useful when working with indirect object references
    /// in content streams or resource dictionaries.
    pub fn resolve_object(&self, obj: &Object) -> Result<Object> {
        if let Some(ref_val) = obj.as_reference() {
            self.load_object(ref_val)
        } else {
            Ok(obj.clone())
        }
    }

    /// Look up a font from the per-document `font_cache`, parsing and inserting
    /// on a cache miss. Used by the page renderer so that `FontInfo::from_dict`
    /// (which decodes widths, CID maps, ToUnicode CMaps, and extracts embedded
    /// font bytes) is called at most once per PDF object reference, even when
    /// multiple pages share the same font resources.
    #[cfg(feature = "rendering")]
    pub fn get_or_load_font_for_rendering(
        &self,
        font_obj: &Object,
    ) -> Result<Arc<crate::fonts::FontInfo>> {
        if let Some(font_ref) = font_obj.as_reference() {
            let cached = self.font_cache.lock_or_recover().get(&font_ref).cloned();
            if let Some(arc) = cached {
                return Ok(arc);
            }
        }
        let resolved = self.deref_object_for_inks(font_obj)?;
        let info = crate::fonts::FontInfo::from_dict(&resolved, self)?;
        let arc = Arc::new(info);
        if let Some(font_ref) = font_obj.as_reference() {
            self.font_cache
                .lock_or_recover()
                .insert(font_ref, Arc::clone(&arc));
        }
        Ok(arc)
    }
}
