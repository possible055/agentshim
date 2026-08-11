use super::*;

impl<'doc> TextExtractor<'doc> {
    pub(super) fn execute_marked_content_operator(&mut self, op: Operator) -> Result<()> {
        match op {
            Operator::BeginMarkedContent { tag } => {
                // Flush the Tj span buffer at the marked-content boundary
                // (ISO 32000-1:2008 §14.6). Without this, consecutive Tj
                // operators that straddle a BMC/BDC/EMC boundary get
                // glued into a single span whose `mcid` reflects only
                // the FIRST Tj — fusing two structurally-distinct
                // elements and breaking every downstream consumer that
                // relies on MCID identity (structure-tree reading
                // order, tree-scope ActualText suppression,
                // table-cell membership).
                self.flush_tj_span_buffer()?;
                if tag == "ReversedChars" {
                    self.saw_reversed_chars = true;
                }
                // BMC doesn't have properties, but the tag can indicate artifacts
                let is_artifact = tag == "Artifact";
                // InDesign placed-PDF figure region (see MarkedContentContext::is_placed_pdf).
                let is_placed_pdf = tag == "PlacedPDF";
                self.marked_content_stack.push(MarkedContentContext {
                    tag: tag.clone(),
                    is_artifact,
                    artifact_type: None, // No artifact classification; None for backward compatibility
                    actual_text: None,   // BMC doesn't have ActualText
                    actual_text_emitted: false,
                    expansion: None,          // BMC doesn't have expansion
                    is_excluded_layer: false, // BMC cannot carry OCG properties
                    is_placed_pdf,
                    own_mcid: None, // BMC carries no MCID
                });
                self.update_artifact_state();
                self.update_layer_state();

                if is_artifact {
                    log::debug!("Entered /Artifact marked content (BMC, no subtype)");
                }
            }

            Operator::BeginMarkedContentDict { tag, properties } => {
                // See `BeginMarkedContent` for the rationale; same
                // reasoning applies to BDC.
                self.flush_tj_span_buffer()?;
                // BDC can have properties including MCID, artifact indicators, ActualText, and expansion
                // Properties can be an inline dictionary or a name referencing /Properties resource
                let mut actual_text = None;
                let mut artifact_type = None;
                let mut expansion = None;
                let mut own_mcid: Option<u32> = None;

                let mut is_excluded_layer = false;

                if let Some(props_dict) = self.resolve_bdc_properties(&properties) {
                    if let Some(mcid_obj) = props_dict.get("MCID") {
                        if let Some(mcid) = mcid_obj.as_integer() {
                            own_mcid = Some(mcid as u32);
                            self.current_mcid = Some(mcid as u32);
                            log::debug!("Entered marked content with MCID: {}", mcid);
                        }
                    }

                    if let Some(actual_text_obj) = props_dict.get("ActualText") {
                        if let Some(text_bytes) = actual_text_obj.as_string() {
                            actual_text = Some(Self::decode_pdf_text_string(text_bytes));
                            log::debug!("Marked content has ActualText: {:?}", actual_text);
                            // Record that this MCID's in-stream
                            // /ActualText is the authoritative
                            // replacement (MC-scope wins over any
                            // ancestor's struct-tree-scope
                            // /ActualText).
                            if let Some(mcid) = self.current_mcid {
                                self.mc_actualtext_mcids.insert(mcid);
                            }
                        }
                    }

                    if let Some(expansion_obj) = props_dict.get("E") {
                        if let Some(text_bytes) = expansion_obj.as_string() {
                            expansion = Some(Self::decode_pdf_text_string(text_bytes));
                            log::debug!("Marked content has expansion /E: {:?}", expansion);
                        }
                    }

                    if tag == "Artifact" {
                        artifact_type = Self::parse_artifact_type(&props_dict);
                    }

                    // OCG / OCMD (Optional Content) filtering.
                    // Per ISO 32000-1:2008 Section 8.11.2:
                    //  - Direct OCG: << /Type /OCG /Name /LayerName >>
                    //  - OCMD:       << /Type /OCMD /OCGs [refs...] /P /policy >>
                    if tag == "OC" && !self.excluded_layers.is_empty() {
                        is_excluded_layer = self.check_ocg_excluded(&props_dict);
                    }
                }

                // Check if this is an artifact (per PDF Spec Section 14.6)
                let is_artifact = tag == "Artifact";
                // InDesign placed-PDF figure region (see MarkedContentContext::is_placed_pdf).
                let is_placed_pdf = tag == "PlacedPDF";
                self.marked_content_stack.push(MarkedContentContext {
                    tag: tag.clone(),
                    is_artifact,
                    artifact_type: artifact_type.clone(),
                    actual_text,
                    actual_text_emitted: false,
                    expansion,
                    is_excluded_layer,
                    is_placed_pdf,
                    own_mcid,
                });
                self.update_artifact_state();
                self.update_layer_state();

                if is_artifact {
                    if let Some(ref atype) = artifact_type {
                        log::debug!("Entered /Artifact marked content: {:?}", atype);
                    } else {
                        log::debug!("Entered /Artifact marked content (no type specified)");
                    }
                }
            }

            Operator::EndMarkedContent => {
                // Flush the Tj span buffer at the marked-content
                // boundary; see `BeginMarkedContent` for the
                // rationale.
                self.flush_tj_span_buffer()?;
                // EMC ends the current marked content sequence.
                // Pop the stack THEN restore `current_mcid` from the
                // nearest enclosing BDC that carried `/MCID` — per
                // ISO 32000-1:2008 §14.6, marked-content sequences
                // nest, and a `Tj` issued after an inner EMC must
                // attribute to its enclosing scope. Blanking
                // `current_mcid` here would orphan that `Tj`'s span
                // (MAJOR-1 regression #...).
                if !self.marked_content_stack.is_empty() {
                    self.marked_content_stack.pop();
                    self.update_artifact_state();
                    self.update_layer_state();
                }
                let restored = self
                    .marked_content_stack
                    .iter()
                    .rev()
                    .find_map(|ctx| ctx.own_mcid);
                if let Some(prev) = self.current_mcid {
                    log::debug!(
                        "Exited marked content with MCID: {} -> restoring to {:?}",
                        prev,
                        restored
                    );
                }
                self.current_mcid = restored;
            }

            // XObject operator - Process Form XObjects for text extraction
            _ => unreachable!("non-marked-content operator delegated to marked-content handler"),
        }
        Ok(())
    }
}
