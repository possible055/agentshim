use super::*;

impl<'doc> TextExtractor<'doc> {
    /// Calculate adaptive TJ offset threshold based on font size and text justification.
    ///
    /// When `use_adaptive_tj_threshold` is enabled, this method calculates the TJ offset
    /// threshold dynamically using the formula:
    ///
    /// ```text
    /// adaptive_threshold = -(space_width * font_size * margin_ratio) / 1000
    /// ```
    ///
    /// Where `margin_ratio` is adjusted based on justified vs normal text detection:
    /// - **Justified text** (high CV > 0.5): Uses 3× the normal ratio (conservative)
    ///   to prevent false space insertions from arbitrary TJ offsets
    /// - **Normal text** (low CV ≤ 0.5): Uses the default ratio (aggressive)
    ///
    /// # Adaptive Threshold Enhancement
    ///
    /// Per ISO 32000-1:2008 Section 9.4.4, justified text uses arbitrary TJ offsets to
    /// distribute whitespace. This method detects justified text through statistical
    /// analysis (coefficient of variation) and adapts the threshold accordingly.
    ///
    /// # Fallback Behavior
    ///
    /// If adaptive thresholds are disabled, this method returns the static
    /// `space_insertion_threshold` from the configuration.
    ///
    /// # PDF Spec Compliance
    ///
    /// Per Section 9.10: "Determining word boundaries is not specified by PDF."
    /// This method uses only spec-defined TJ values and geometric positions.
    pub(super) fn calculate_adaptive_tj_threshold(&self) -> f32 {
        // Check if adaptive thresholds are enabled
        if !self.config.use_adaptive_tj_threshold {
            return self.config.space_insertion_threshold;
        }

        // Get current text state
        let state = self.state_stack.current();

        // ==============================================================================
        // FONT-AWARE ADAPTIVE THRESHOLD WITH JUSTIFIED TEXT DETECTION
        // (ISO 32000-1:2008 Section 9.4.4, 9.6.3, 9.10)
        // ==============================================================================

        let font_size = state.font_size;

        // Get font from current text state to access space glyph width
        // ISO 32000-1:2008 Section 9.6.3: Font metrics (glyph widths)
        let space_width_units = state
            .font_name
            .as_ref()
            .and_then(|name| self.fonts.get(name))
            .map(|font| font.get_space_glyph_width())
            .unwrap_or(250.0); // Fallback: Times-Roman typical space width

        // Detect justified vs normal text
        let (is_justified, cv) = self.analyze_tj_distribution();

        // Adjust margin ratio based on text justification
        // Justified text: use 3× conservative ratio (reduce false spaces)
        // Normal text: use default ratio
        let margin_ratio = if is_justified {
            self.config.word_margin_ratio * 3.0 // Conservative for justified
        } else {
            self.config.word_margin_ratio // Normal for non-justified
        };

        // Calculate threshold: negative offset required to trigger space insertion
        // Normalized by 1000 (PDF spec font units are 1/1000em)
        let adaptive_threshold = -((space_width_units * font_size * margin_ratio) / 1000.0);

        log::debug!(
            "TJ threshold: {} (justified={}, cv={:.2}, margin_ratio={:.3}, ISO 32000-1 §9.4.4)",
            adaptive_threshold,
            is_justified,
            cv,
            margin_ratio
        );

        adaptive_threshold
    }

    /// Analyze TJ offset distribution to detect justified vs normal text.
    ///
    /// This method performs statistical analysis on collected TJ offsets to determine
    /// if the document uses justified alignment. Justified text has high variance in TJ
    /// offsets (to distribute whitespace), while normally-spaced text has low variance.
    ///
    /// # Returns
    ///
    /// A tuple `(is_justified: bool, coefficient_of_variation: f32)` where:
    /// - `is_justified`: true if CV > 0.5 (high variance = justified text)
    /// - `coefficient_of_variation`: standard deviation / mean (normalized spread)
    ///
    /// # Algorithm
    ///
    /// Per ISO 32000-1:2008 Section 9.4.4, TJ array offsets are in font-relative units
    /// (1/1000 of text space). The distribution is analyzed as:
    ///
    /// 1. Calculate mean of all TJ offsets
    /// 2. Calculate variance: average of squared deviations from mean
    /// 3. Calculate standard deviation: sqrt(variance)
    /// 4. Calculate coefficient of variation: std_dev / |mean|
    ///
    /// # Thresholds
    ///
    /// - CV > 0.5: Justified text (high variance in offsets)
    /// - CV ≤ 0.5: Normal text (consistent spacing)
    ///
    /// # PDF Spec Compliance
    ///
    /// Per ISO 32000-1:2008 Section 9.10 ("Extraction of Text Content"):
    /// "Determining word boundaries is not specified by PDF." This method uses only
    /// spec-defined TJ offset values to infer text characteristics, not semantic assumptions.
    pub(super) fn analyze_tj_distribution(&self) -> (bool, f32) {
        let n = self.tj_offset_history.len();
        if n == 0 {
            return (false, 0.0);
        }

        // Use the accumulators when current; recompute from the slice if the
        // history was replaced wholesale (same sum order → same result).
        let (sum, sum_sq) = if self.tj_stats_len == n {
            (self.tj_sum, self.tj_sum_sq)
        } else {
            let mut s = 0.0f64;
            let mut sq = 0.0f64;
            for &x in &self.tj_offset_history {
                let x = x as f64;
                s += x;
                sq += x * x;
            }
            (s, sq)
        };

        let nf = n as f64;
        let mean = sum / nf;
        // Variance from accumulators: E[x²] − E[x]². Clamp to ≥0 to absorb
        // floating-point cancellation when the spread is tiny.
        let variance = ((sum_sq / nf) - mean * mean).max(0.0);
        let std_dev = variance.sqrt();

        // Coefficient of variation (normalized spread); guard zero mean.
        let cv = if mean.abs() > 0.001 {
            (std_dev / mean.abs()) as f32
        } else {
            0.0
        };

        let is_justified = cv > 0.5;

        log::debug!(
            "TJ distribution analysis: mean={:.2}, std_dev={:.2}, cv={:.2}, justified={}",
            mean,
            std_dev,
            cv,
            is_justified
        );

        (is_justified, cv)
    }

    /// Update the artifact state based on the marked content stack.
    ///
    /// This method computes whether we're currently inside an artifact region
    /// by checking if any ancestor in the marked_content_stack has is_artifact=true.
    /// Per PDF Spec Section 14.6, artifact content should be excluded from text extraction.
    ///
    /// # Performance
    ///
    /// This is O(n) where n is the depth of the marked content stack (typically 1-5).
    /// Called each time a marked content boundary is crossed (BMC/BDC/EMC).
    pub(super) fn update_artifact_state(&mut self) {
        // True if ANY ancestor in the stack is an artifact
        self.inside_artifact = self.marked_content_stack.iter().any(|ctx| ctx.is_artifact);
    }

    /// Update the excluded-layer state based on the marked content stack.
    ///
    /// True if any ancestor in the stack is an excluded OCG layer.
    /// Called each time a marked content boundary is crossed (BMC/BDC/EMC).
    pub(super) fn update_layer_state(&mut self) {
        self.inside_excluded_layer = self
            .marked_content_stack
            .iter()
            .any(|ctx| ctx.is_excluded_layer);
        self.inside_placed_pdf = self
            .marked_content_stack
            .iter()
            .any(|ctx| ctx.is_placed_pdf);
    }

    /// Whether content emission should be suppressed.
    ///
    /// Returns true when the current graphics/marked-content state means
    /// extracted text should be discarded. Currently checks:
    /// - Inside an excluded OCG layer (`inside_excluded_layer`)
    /// - Inside an excluded ink / separation color space (`inside_excluded_ink`)
    /// - Inside an InDesign `/PlacedPDF` figure region (`inside_placed_pdf`)
    ///
    /// Note: artifact filtering is handled separately via span metadata and
    /// downstream filtering, so `inside_artifact` is intentionally not checked here.
    pub(super) fn is_content_suppressed(&self) -> bool {
        self.inside_excluded_layer
            || self.inside_excluded_ink
            || (self.inside_placed_pdf && !self.placed_pdf_keep)
    }

    /// Decide whether `/PlacedPDF` text should be kept (not suppressed) for a page.
    ///
    /// Cheap read-only pre-scan of the page content stream. Text-show operands
    /// (`Tj`/`TJ`/`'`/`"`) are bucketed by whether they are emitted INSIDE a
    /// `/PlacedPDF` marked-content scope or OUTSIDE it, and the placed bucket is
    /// KEPT (not suppressed) unless it looks like a decorative/duplicate overlay:
    ///
    /// 1. Too little placed text (`< MIN_PLACED_CHARS`) -> suppress. A stray
    ///    placed logo or figure caption is not the page body.
    /// 2. Placed text clearly dominates the page (non-placed text is a small
    ///    minority, ~3:1) -> keep. The publisher placed the whole article body as
    ///    one `/PlacedPDF` and left only a running header outside (MATEC Web of
    ///    Conferences).
    /// 3. Placed text is substantial but the non-placed text is comparable or
    ///    larger -> keep ONLY when the placed words are mostly NOT already present
    ///    outside. A placed region that mostly repeats the surrounding text is a
    ///    draft galley / overlay copy and stays suppressed (PMC8100493, the de-dup
    ///    win); one carrying mostly-unique words is the page's real placed content
    ///    (e.g. an InDesign figure that holds the page's labels and body text, as
    ///    on placed floor-plan / marketing spreads) and must be kept.
    ///
    /// Conservative on purpose: when placed text lives in a nested XObject the
    /// page-stream scan undercounts it and gate 1 falls back to suppression (the
    /// prior behaviour).
    pub(super) fn placed_pdf_text_dominates(content_stream: &[u8]) -> bool {
        // Gate: only pages that actually carry the InDesign tag pay for a parse.
        if !content_stream
            .windows(b"PlacedPDF".len())
            .any(|w| w == b"PlacedPDF")
        {
            return false;
        }
        let Ok(operators) = parse_content_stream(content_stream) else {
            return false;
        };
        // A substantial placed body; below this a placed region is treated as a
        // decorative figure, not the page's logical content.
        const MIN_PLACED_CHARS: usize = 800;
        // Keep placed text whose words are mostly unique; suppress it once a
        // majority is also present in the non-placed text (a duplicate overlay).
        const MAX_DUP_FRACTION: f64 = 0.5;

        let mut placed_stack: Vec<bool> = Vec::new();
        let mut placed_chars: usize = 0;
        let mut other_chars: usize = 0;
        let mut placed_txt: Vec<u8> = Vec::new();
        let mut other_txt: Vec<u8> = Vec::new();
        let inside = |stack: &[bool]| stack.iter().any(|&p| p);
        for op in &operators {
            match op {
                Operator::BeginMarkedContent { tag }
                | Operator::BeginMarkedContentDict { tag, .. } => {
                    placed_stack.push(tag == "PlacedPDF");
                }
                Operator::EndMarkedContent => {
                    placed_stack.pop();
                }
                Operator::Tj { text } | Operator::Quote { text } => {
                    let (chars, txt) = if inside(&placed_stack) {
                        (&mut placed_chars, &mut placed_txt)
                    } else {
                        (&mut other_chars, &mut other_txt)
                    };
                    *chars += text.len();
                    txt.extend_from_slice(text);
                    txt.push(b' ');
                }
                Operator::DoubleQuote { text, .. } => {
                    let (chars, txt) = if inside(&placed_stack) {
                        (&mut placed_chars, &mut placed_txt)
                    } else {
                        (&mut other_chars, &mut other_txt)
                    };
                    *chars += text.len();
                    txt.extend_from_slice(text);
                    txt.push(b' ');
                }
                Operator::TJ { array } => {
                    let (chars, txt) = if inside(&placed_stack) {
                        (&mut placed_chars, &mut placed_txt)
                    } else {
                        (&mut other_chars, &mut other_txt)
                    };
                    for e in array {
                        if let TextElement::String(s) = e {
                            *chars += s.len();
                            txt.extend_from_slice(s);
                        }
                    }
                    txt.push(b' ');
                }
                _ => {}
            }
        }

        // Gate 1: too little placed text -> decorative figure, suppress.
        if placed_chars < MIN_PLACED_CHARS {
            return false;
        }
        // Gate 2: placed text dominates the page -> whole-body placed, keep.
        if other_chars.saturating_mul(3) < placed_chars {
            return true;
        }
        // Gate 3: placed text is substantial but the outside text is comparable
        // or larger. Keep it unless a majority of the placed words also appear
        // outside (a duplicate overlay). Tokenising here (behind gates 1 and 2)
        // keeps the common single-column path allocation-free.
        Self::text_duplication_fraction(&placed_txt, &other_txt) < MAX_DUP_FRACTION
    }

    /// Fraction of alphanumeric word tokens in `a` (counting repeats) that also
    /// occur anywhere in `b`. Words are lowercased runs of >= 2 alphanumeric
    /// bytes; punctuation and single characters are ignored. Returns 0.0 when `a`
    /// has no such tokens (nothing to be a duplicate of).
    pub(super) fn text_duplication_fraction(a: &[u8], b: &[u8]) -> f64 {
        fn tokens(bytes: &[u8]) -> Vec<Vec<u8>> {
            let mut out = Vec::new();
            let mut cur = Vec::new();
            for &c in bytes {
                if c.is_ascii_alphanumeric() {
                    cur.push(c.to_ascii_lowercase());
                } else if !cur.is_empty() {
                    if cur.len() >= 2 {
                        out.push(std::mem::take(&mut cur));
                    } else {
                        cur.clear();
                    }
                }
            }
            if cur.len() >= 2 {
                out.push(cur);
            }
            out
        }
        let a_tokens = tokens(a);
        if a_tokens.is_empty() {
            return 0.0;
        }
        let b_set: std::collections::HashSet<Vec<u8>> = tokens(b).into_iter().collect();
        let shared = a_tokens.iter().filter(|t| b_set.contains(*t)).count();
        shared as f64 / a_tokens.len() as f64
    }

    /// Parse artifact type and subtype from artifact properties dictionary.
    ///
    /// Per PDF Spec Section 14.8.2.2, artifacts have optional /Type and /Subtype entries:
    /// - /Type: Pagination, Layout, Page, or Background
    /// - /Subtype: For Pagination artifacts: Header, Footer, Watermark, etc.
    ///
    /// # Arguments
    ///
    /// * `props_dict` - The properties dictionary from BDC operator
    ///
    /// # Returns
    ///
    /// The classified artifact type, or None if no type is specified
    pub(super) fn parse_artifact_type(
        props_dict: &HashMap<String, Object>,
    ) -> Option<ArtifactType> {
        // Extract /Type entry (PDF Spec Section 14.8.2.2)
        let artifact_type_name = props_dict
            .get("Type")
            .and_then(|obj| obj.as_name())
            .map(|s| s.to_lowercase());

        // Extract /Subtype entry for Pagination artifacts
        let subtype_name = props_dict
            .get("Subtype")
            .and_then(|obj| obj.as_name())
            .map(|s| s.to_lowercase());

        match artifact_type_name.as_deref() {
            Some("pagination") => {
                let subtype = match subtype_name.as_deref() {
                    Some("header") => PaginationSubtype::Header,
                    Some("footer") => PaginationSubtype::Footer,
                    Some("watermark") => PaginationSubtype::Watermark,
                    Some("pagenumber") | Some("page") => PaginationSubtype::PageNumber,
                    _ => PaginationSubtype::Other,
                };
                Some(ArtifactType::Pagination(subtype))
            }
            Some("layout") => Some(ArtifactType::Layout),
            Some("page") => Some(ArtifactType::Page),
            Some("background") => Some(ArtifactType::Background),
            None => {
                // No /Type specified - check if /Subtype alone indicates pagination
                // Some PDFs use /Subtype without /Type
                match subtype_name.as_deref() {
                    Some("header") => Some(ArtifactType::Pagination(PaginationSubtype::Header)),
                    Some("footer") => Some(ArtifactType::Pagination(PaginationSubtype::Footer)),
                    Some("watermark") => {
                        Some(ArtifactType::Pagination(PaginationSubtype::Watermark))
                    }
                    _ => None,
                }
            }
            _ => None, // Unknown type
        }
    }

    /// Decode a PDF text string (handles UTF-16BE/LE with BOM and PDFDocEncoding).
    ///
    /// Thin delegate to [`crate::optional_content::decode_pdf_text_string`] — that
    /// module owns the canonical implementation shared with the rendering path
    /// (UTF-16BE/LE with BOM, PDFDocEncoding fallback per ISO 32000-1:2008 §7.9.2).
    pub(super) fn decode_pdf_text_string(bytes: &[u8]) -> String {
        crate::optional_content::decode_pdf_text_string(bytes)
    }

    /// Resolve BDC properties: can be an inline dictionary or a name referencing /Properties resource.
    ///
    /// Thin delegate to [`crate::optional_content::resolve_bdc_properties`].
    /// Passing `self.document` as `Option` lets the inline-dict fast path work
    /// even on a freshly-constructed extractor with no document attached (used
    /// by unit tests).
    pub(super) fn resolve_bdc_properties(
        &self,
        properties: &Object,
    ) -> Option<std::collections::HashMap<String, Object>> {
        crate::optional_content::resolve_bdc_properties(
            properties,
            self.resources.as_ref(),
            self.document,
        )
    }

    /// Resolve a named color space from the /Resources /ColorSpace dictionary.
    ///
    /// PDF content streams reference color spaces by name (e.g. `cs /CS1`).
    /// Device color spaces like "DeviceRGB" are built-in, but Separation and
    /// DeviceN color spaces live in the page resources:
    ///
    /// ```text
    /// /Resources << /ColorSpace << /CS1 [/Separation /PANTONE_Red /DeviceCMYK ...] >> >>
    /// ```
    ///
    /// Returns the resolved color space array if the name refers to a resource entry.
    pub(super) fn resolve_color_space(&self, name: &str) -> Option<Vec<Object>> {
        let resources = self.resources.as_ref()?;
        let res_dict = if let Some(res_ref) = resources.as_reference() {
            self.document?.load_object(res_ref).ok()?
        } else {
            resources.clone()
        };
        let res_dict = res_dict.as_dict()?;
        let cs_dict_obj = res_dict.get("ColorSpace")?;
        let cs_dict = if let Some(r) = cs_dict_obj.as_reference() {
            self.document?.load_object(r).ok()?
        } else {
            cs_dict_obj.clone()
        };
        let cs_dict = cs_dict.as_dict()?;
        let cs_obj = cs_dict.get(name)?;
        let resolved = if let Some(r) = cs_obj.as_reference() {
            self.document?.load_object(r).ok()?
        } else {
            cs_obj.clone()
        };
        resolved.as_array().cloned()
    }

    /// Check if a color space name refers to an excluded ink.
    ///
    /// Resolves the color space from resources and checks:
    /// - `[/Separation /InkName /AlternateCS /TintTransform]` — single ink name
    /// - `[/DeviceN [/Ink1 /Ink2 ...] /AlternateCS /TintTransform]` — multiple ink names
    ///
    /// Returns true if any ink name in the color space matches `excluded_inks`.
    ///
    /// **Note:** For DeviceN, this is all-or-nothing — if any ink matches, the
    /// entire color space is treated as excluded. Tint values are not evaluated.
    pub(super) fn is_excluded_ink_color_space(&self, name: &str) -> bool {
        if self.excluded_inks.is_empty() {
            return false;
        }
        if let Some(cs_array) = self.resolve_color_space(name) {
            if cs_array.len() >= 2 {
                if let Some(cs_type) = cs_array[0].as_name() {
                    // §8.6.6.2 / §8.6.6.3: the colorant slot (Separation's
                    // ink-name, DeviceN's names array) can be an indirect
                    // reference. Some subsetters share the names list across
                    // multiple DeviceN spaces, emitting
                    // `[/DeviceN 4 0 R /DeviceCMYK <attrs>]` where `4 0 R`
                    // points to the actual names list. Resolve before
                    // pattern-matching.
                    let deref = |obj: &Object| -> Object {
                        match (obj.as_reference(), self.document) {
                            (Some(r), Some(d)) => d.load_object(r).unwrap_or_else(|_| obj.clone()),
                            _ => obj.clone(),
                        }
                    };
                    match cs_type {
                        "Separation" => {
                            // [/Separation /InkName /AlternateCS /TintTransform]
                            let name_obj = deref(&cs_array[1]);
                            if let Some(ink_name) = name_obj.as_name() {
                                return self.excluded_inks.contains(ink_name);
                            }
                        }
                        "DeviceN" => {
                            // [/DeviceN <names-array> /AlternateCS /TintTransform <attrs>]
                            let names_obj = deref(&cs_array[1]);
                            if let Some(ink_names) = names_obj.as_array() {
                                return ink_names.iter().any(|obj| {
                                    obj.as_name()
                                        .map(|n| self.excluded_inks.contains(n))
                                        .unwrap_or(false)
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        false
    }

    /// Check whether a BDC properties dict represents an excluded OCG or OCMD.
    ///
    /// Thin delegate to [`crate::optional_content::check_ocg_excluded`] — that
    /// module is the single source of truth for OCG/OCMD evaluation, including
    /// OCMD `/P` visibility-policy handling.
    pub(super) fn check_ocg_excluded(
        &self,
        props_dict: &std::collections::HashMap<String, Object>,
    ) -> bool {
        let doc = match self.document {
            Some(d) => d,
            None => return false,
        };
        crate::optional_content::check_ocg_excluded(props_dict, doc, &self.excluded_layers)
    }

    /// Get current ActualText from marked content stack (PDF Spec Section 14.9.4).
    ///
    /// Searches from the innermost marked content context outward, returning
    /// the first ActualText found. If no ActualText is defined, returns None.
    ///
    /// ActualText provides the exact text representation for content that's
    /// represented non-standardly, such as ligatures (fi, fl, ffi, ffl) or
    /// decorated glyphs.
    #[cfg(test)]
    pub(super) fn get_current_actual_text(&self) -> Option<String> {
        self.marked_content_stack
            .iter()
            .rev() // Search from innermost (most recent) context
            .find_map(|ctx| ctx.actual_text.clone())
    }

    /// Return the innermost active `/ActualText`, alongside a flag
    /// indicating whether it has ALREADY been emitted in the current
    /// MC scope.
    ///
    /// Per ISO 32000-1:2008 §14.9.4 the `/ActualText` of a marked-
    /// content sequence replaces the ENTIRE sequence — even if the
    /// sequence contains multiple `Tj` / `TJ` operators the replacement
    /// is emitted ONCE.
    ///
    /// Returns `(text, already_emitted)`:
    /// - `(Some(text), false)` on the FIRST show-text inside a scope
    ///   that carries `/ActualText` — the caller should emit `text`
    ///   AND mark the scope's `actual_text_emitted` via
    ///   [`Self::mark_actual_text_emitted`].
    /// - `(Some(text), true)` on subsequent show-text operators inside
    ///   the same scope — the caller must suppress emission entirely
    ///   (no raw glyphs, no replacement). The text matrix advance
    ///   still runs so positioning stays consistent.
    /// - `(None, _)` when no `/ActualText` is active.
    pub(super) fn peek_current_actual_text(&self) -> (Option<String>, bool) {
        for ctx in self.marked_content_stack.iter().rev() {
            if let Some(ref text) = ctx.actual_text {
                return (Some(text.clone()), ctx.actual_text_emitted);
            }
        }
        (None, false)
    }

    /// Mark the innermost scope's `/ActualText` as emitted. See
    /// [`Self::peek_current_actual_text`].
    pub(super) fn mark_actual_text_emitted(&mut self) {
        for ctx in self.marked_content_stack.iter_mut().rev() {
            if ctx.actual_text.is_some() {
                ctx.actual_text_emitted = true;
                return;
            }
        }
    }
}
