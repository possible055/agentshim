use super::*;

impl PageRenderer {
    /// Render an XObject (image or form).
    /// Resolve the `/Subtype` name of the named XObject in the active
    /// resources without rendering it. Returns `Some("Form")`,
    /// `Some("Image")`, etc., or `None` when the lookup fails or the
    /// XObject lacks a `/Subtype`. Used by the `Do` operator dispatcher
    /// to pick the correct post-Do colour-lane modulators per ISO
    /// 32000-1 §11.4.7 (Image XObjects paint with outer gs; Form
    /// XObjects run their own operators with their own gs).
    pub(super) fn xobject_subtype(
        &self,
        name: &str,
        resources: &Object,
        doc: &PdfDocument,
    ) -> Option<String> {
        let res_dict = resources.as_dict()?;
        let xobj_entry = res_dict.get("XObject")?;
        let xobjects_obj = doc.resolve_object(xobj_entry).ok()?;
        let xobjects = xobjects_obj.as_dict()?;
        let xobj_ref_obj = xobjects.get(name)?;
        let xobj = doc.resolve_object(xobj_ref_obj).ok()?;
        if let Object::Stream { ref dict, .. } = xobj {
            return dict
                .get("Subtype")
                .and_then(|o| o.as_name())
                .map(String::from);
        }
        None
    }

    pub(super) fn render_xobject(
        &mut self,
        pixmap: &mut Pixmap,
        name: &str,
        transform: Transform,
        gs: &GraphicsState,
        resources: &Object,
        doc: &PdfDocument,
        page_num: usize,
        clip_mask: Option<&tiny_skia::Mask>,
    ) -> Result<()> {
        // Get XObject from resources
        if let Object::Dictionary(res_dict) = resources {
            // PDF spec uses "XObject" (singular)
            if let Some(xobj_entry) = res_dict.get("XObject") {
                let xobjects_obj = doc.resolve_object(xobj_entry)?;
                if let Some(xobjects) = xobjects_obj.as_dict() {
                    if let Some(xobj_ref_obj) = xobjects.get(name) {
                        // Resolve reference if needed
                        let xobj = doc.resolve_object(xobj_ref_obj)?;
                        let xobj_ref = xobj_ref_obj.as_reference();
                        log::debug!("Resolved XObject '{}' type: {:?}", name, xobj);

                        if let Object::Stream { ref dict, .. } = xobj {
                            if let Some(smask) = dict.get("SMask") {
                                log::debug!("Image has SMask: {:?}", smask);
                            }
                            if let Some(mask) = dict.get("Mask") {
                                log::debug!("Image has Mask: {:?}", mask);
                            }
                            if let Some(imask) = dict.get("ImageMask") {
                                log::debug!("Image is ImageMask: {:?}", imask);
                            }
                            // Check subtype
                            if let Some(subtype) = dict.get("Subtype").and_then(|o| o.as_name()) {
                                match subtype {
                                    "Image" => {
                                        // ImageMask XObjects (1-bit stencil painted with
                                        // the current fill colour) take their fill from
                                        // graphics state, not from the pixel data. Route
                                        // that fill through the resolution pipeline so a
                                        // Type 4 Separation fill paints the mask with the
                                        // function-evaluated tint rather than the legacy
                                        // `1 - tint` fallback.
                                        //
                                        // Standard images (`/ImageMask` absent or false)
                                        // carry their colour in the pixel data and do
                                        // not interact with the pipeline; they pass
                                        // straight through to `render_image`.
                                        let is_image_mask = dict
                                            .get("ImageMask")
                                            .map(|o| matches!(o, Object::Boolean(true)))
                                            .unwrap_or(false);
                                        if is_image_mask {
                                            let spliced = self.pipeline_resolve_paint_gs(
                                                doc,
                                                gs,
                                                PipelinePaintKind::ImageMask,
                                            );
                                            let render_gs: &GraphicsState =
                                                spliced.as_ref().unwrap_or(gs);
                                            if let Err(e) = self.render_image_mask(
                                                pixmap, &xobj, xobj_ref, transform, doc, clip_mask,
                                                render_gs,
                                            ) {
                                                log::warn!(
                                                    "Skipping unrenderable ImageMask XObject '{}': {}",
                                                    name,
                                                    e
                                                );
                                            }
                                        } else {
                                            let smask = dict.get("SMask").cloned();
                                            let mask = dict.get("Mask").cloned();
                                            if let Err(e) = self.render_image(
                                                pixmap, &xobj, xobj_ref, transform, doc, clip_mask,
                                                smask, mask, gs,
                                            ) {
                                                log::warn!(
                                                    "Skipping unrenderable image XObject '{}': {}",
                                                    name,
                                                    e
                                                );
                                            }
                                        }
                                    }
                                    "Form" => {
                                        log::debug!("XObject '{}' is a Form", name);
                                        // Decoded stream data
                                        let stream_data = if let Some(r) = xobj_ref {
                                            doc.decode_stream_with_encryption(&xobj, r)?
                                        } else {
                                            xobj.decode_stream_data()?
                                        };

                                        // Form XObjects can have their own Resources dictionary.
                                        let form_resources =
                                            dict.get("Resources").unwrap_or(resources);

                                        // Save current fonts and load form-specific fonts
                                        let old_fonts = self.fonts.clone();
                                        let old_cs = self.color_spaces.clone();
                                        self.load_resources(doc, form_resources)?;

                                        if let Err(e) = self.render_form_xobject(
                                            pixmap,
                                            &dict,
                                            &stream_data,
                                            transform,
                                            doc,
                                            page_num,
                                            form_resources,
                                        ) {
                                            log::warn!(
                                                "Skipping malformed Form XObject '{}': {}",
                                                name,
                                                e
                                            );
                                        }

                                        // Restore caches
                                        self.fonts = old_fonts;
                                        self.color_spaces = old_cs;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
