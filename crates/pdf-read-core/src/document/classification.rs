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

    /// Gather per-page classification signals from pdf_oxide
    /// **internals** (00-common-foundation §9 — never the flattened
    /// output string). Returns the signals plus the enriched T0.5
    /// quality-gate verdict (research §3a) computed from the *same*
    /// single span extraction (no double work). Pure inspection.
    fn gather_page_signals(
        &self,
        page: usize,
    ) -> Result<(
        crate::extractors::auto::PageSignals,
        Option<crate::extractors::auto::ReasonCode>,
    )> {
        use crate::content::{Operator, TextElement};
        use crate::extractors::auto::{ImageCodecClass, PageSignals, ProducerPrior};
        use crate::extractors::ImageData;

        let (llx, lly, urx, ury) = self.get_page_media_box(page)?;
        let rot = self.get_page_rotation(page).unwrap_or(0);
        let (mut pw, mut ph) = ((urx - llx).abs(), (ury - lly).abs());
        if rot % 180 != 0 {
            std::mem::swap(&mut pw, &mut ph);
        }
        let page_area = (pw * ph).max(1.0);
        let (px0, py0, px1, py1) = (llx.min(urx), lly.min(ury), llx.max(urx), lly.max(ury));

        // ── native text (artifact spans downweighted — cases G/T) ──
        let spans = self.extract_spans(page).unwrap_or_default();
        let mut text = String::new();
        let mut glyphs = 0usize;
        let mut text_area = 0.0f32;
        for s in &spans {
            if s.artifact_type.is_some() {
                continue;
            }
            let n = s.text.chars().count();
            if n == 0 {
                continue;
            }
            glyphs += n;
            text.push_str(&s.text);
            text.push(' ');
            text_area += Self::rect_isect_area(&s.bbox, px0, py0, px1, py1);
        }
        let text_area_ratio = (text_area / page_area).clamp(0.0, 1.0);

        let chars: Vec<char> = text.chars().collect();
        let total = chars.len().max(1);
        let bad = chars
            .iter()
            .filter(|&&c| {
                c == '\u{FFFD}' || c.is_control() || ('\u{E000}'..='\u{F8FF}').contains(&c)
            })
            .count();
        let garbled_ratio = bad as f32 / total as f32;
        // Word-boundary signals (fragmentation, consecutive-repeat) need
        // real words, not one token per span. Each span above is a raw
        // content-stream text-showing run; in math typesetting every atom
        // ((, ∞, ), a subscript, an operator) is its own span, so joining
        // spans with a forced space makes every span boundary look like a
        // word boundary and inflates the fragmented-word ratio on ordinary
        // dense LaTeX pages until the text-quality gate mistakes them for
        // scans. `extract_words` already does the real glyph/span
        // clustering (adaptive gap thresholds, same-line merge, backtrack
        // guard) that `extract_text` relies on — reuse its output here
        // instead of re-deriving word boundaries from span punctuation.
        let word_text: String = self
            .extract_words(page)
            .unwrap_or_default()
            .into_iter()
            .map(|w| w.text)
            .collect::<Vec<_>>()
            .join(" ");
        let words: Vec<&str> = word_text.split_whitespace().collect();
        let (fragmented_word_ratio, consecutive_repeat_ratio) = if words.is_empty() {
            (0.0, 0.0)
        } else {
            let frag =
                words.iter().filter(|w| w.chars().count() <= 2).count() as f32 / words.len() as f32;
            let rep = words.windows(2).filter(|w| w[0] == w[1]).count() as f32 / words.len() as f32;
            // CJK/Hangul text has no inter-word spaces, so glyph-adjacency
            // clustering naturally produces short (often 1-2 character)
            // tokens — `frag` here is calibrated for space-separated Latin
            // text and would otherwise read ordinary dense CJK prose as
            // fragmented (this ratio directly gates `usable_text` in
            // `classify_from_signals`). The repeat ratio is script-agnostic
            // and stays as computed.
            let frag = if crate::extractors::auto::is_cjk_dominant_text(&word_text) {
                0.0
            } else {
                frag
            };
            (frag, rep)
        };

        // ── images: union coverage (summed → multi-strip, case J) + codec ──
        let images = self.extract_images(page).unwrap_or_default();
        let mut img_area = 0.0f32;
        let mut codec = ImageCodecClass::None;
        for im in &images {
            if let Some(b) = im.bbox() {
                img_area += Self::rect_isect_area(b, px0, py0, px1, py1);
            }
            let c = if im.ccitt_params().is_some() {
                ImageCodecClass::Ccitt
            } else {
                match im.data() {
                    ImageData::Jpeg(_) => ImageCodecClass::Dct,
                    _ => ImageCodecClass::Other,
                }
            };
            codec = match (codec, c) {
                (ImageCodecClass::None, x) => x,
                (_, ImageCodecClass::Ccitt) => ImageCodecClass::Ccitt,
                (cur, _) => cur,
            };
        }
        let image_area_ratio = (img_area / page_area).clamp(0.0, 1.0);

        // ── content-stream ops: Tr-mode-3 ratio (cases C/C2) ──
        let mut invisible = 0usize;
        let mut glyph_bytes = 0usize;
        if let Ok(data) = self.get_page_content_data(page) {
            if let Ok(ops) = crate::content::parse_content_stream(&data) {
                let mut rm: u8 = 0;
                let mut stack: Vec<u8> = Vec::new();
                for op in &ops {
                    match op {
                        Operator::SaveState => stack.push(rm),
                        Operator::RestoreState => {
                            if let Some(p) = stack.pop() {
                                rm = p;
                            }
                        }
                        Operator::Tr { render } => rm = *render,
                        Operator::Tj { text } => {
                            glyph_bytes += text.len();
                            if rm == 3 {
                                invisible += text.len();
                            }
                        }
                        Operator::TJ { array } => {
                            let g: usize = array
                                .iter()
                                .map(|e| match e {
                                    TextElement::String(b) => b.len(),
                                    TextElement::Offset(_) => 0,
                                })
                                .sum();
                            glyph_bytes += g;
                            if rm == 3 {
                                invisible += g;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        let invisible_text_ratio = if glyph_bytes == 0 {
            0.0
        } else {
            invisible as f32 / glyph_bytes as f32
        };

        // ── vector path density (case F) ──
        let path_count = self.extract_paths(page).map(|p| p.len()).unwrap_or(0);
        let vector_path_density = {
            let denom = (path_count + glyphs + images.len()).max(1) as f32;
            (path_count as f32 / denom).clamp(0.0, 1.0)
        };

        // ── structure / producer / empty ──
        let has_reliable_structure = self
            .mark_info()
            .map(|m| m.is_structure_reliable())
            .unwrap_or(false);
        let producer_prior = {
            let p = format!(
                "{} {}",
                self.document_producer().unwrap_or_default(),
                self.document_creator().unwrap_or_default()
            )
            .to_lowercase();
            const SCAN: &[&str] = &[
                "scan",
                "abbyy",
                "tesseract",
                "scansnap",
                "finereader",
                "ocr",
                "lens",
                "camscanner",
                "kofax",
            ];
            const AUTH: &[&str] = &[
                "word",
                "libreoffice",
                "latex",
                "pdftex",
                "chromium",
                "skia",
                "quartz",
                "wkhtmltopdf",
                "pdf_oxide",
                "reportlab",
                "prince",
                "weasyprint",
                "powerpoint",
                "excel",
                "indesign",
            ];
            if SCAN.iter().any(|k| p.contains(k)) {
                ProducerPrior::Scanner
            } else if AUTH.iter().any(|k| p.contains(k)) {
                ProducerPrior::Authoring
            } else {
                ProducerPrior::Unknown
            }
        };
        let page_is_empty = glyphs == 0 && image_area_ratio < 0.01 && path_count == 0;

        let signals = PageSignals {
            text_glyph_count: glyphs,
            text_area_ratio,
            image_area_ratio,
            codec,
            invisible_text_ratio,
            garbled_ratio,
            fragmented_word_ratio,
            consecutive_repeat_ratio,
            vector_path_density,
            has_reliable_structure,
            producer_prior,
            page_is_empty,
        };
        // `text_quality_gate` does its own word-splitting internally; feed
        // it the same word-clustered text as `fragmented_word_ratio` above
        // (not the raw span-joined `text`), for the same reason.
        let gate = crate::extractors::auto::text_quality_gate(&word_text);
        Ok((signals, gate))
    }

    /// Cheap per-page native-text classification without rasterisation. Returns kind +
    /// confidence + typed [`ReasonCode`](crate::extractors::auto::ReasonCode)
    /// + the raw signals (explainable).
    ///
    /// Fails closed on an encrypted-unauthenticated document
    /// (`Error::EncryptedPdf`, case L) — consistent with every other
    /// `extract_*`; the graceful warn+fallback applies to *extraction*
    /// (`extract_page_auto`), not this preflight.
    pub fn classify_page(
        &self,
        page: usize,
    ) -> Result<crate::extractors::auto::PageClassification> {
        use crate::extractors::auto::{
            classify_from_signals, AutoExtractOptions, PageClassification, PageKind,
        };
        if !self.is_authenticated() {
            return Err(Error::EncryptedPdf);
        }
        let (signals, gate) = self.gather_page_signals(page)?;
        let opts = AutoExtractOptions::balanced();
        let (mut kind, mut confidence, mut reason) = classify_from_signals(&signals, &opts);
        // Unusable born-digital text overrides a TextLayer verdict so callers
        // can choose the image path for column scramble, CID garbage, or fragmentation.
        if matches!(kind, PageKind::TextLayer) {
            if let Some(r) = gate {
                kind = PageKind::Scanned;
                confidence = confidence.min(0.80);
                reason = r;
            }
        }
        Ok(PageClassification {
            page,
            kind,
            confidence,
            reason,
            signals,
        })
    }
}
