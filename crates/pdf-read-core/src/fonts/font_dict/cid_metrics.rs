use super::*;

impl FontInfo {
    /// Parse CIDFont /W array for glyph widths.
    ///
    /// Per PDF Spec ISO 32000-1:2008, Section 9.7.4.3, the /W array has two formats:
    /// - `c [w1 w2 ... wn]` - CID c has width w1, c+1 has width w2, etc.
    /// - `cfirst clast w` - CIDs from cfirst to clast all have width w
    ///
    /// These formats can be mixed in a single array.
    ///
    /// # Example /W array
    /// ```pdf
    /// /W [
    ///   1 [500 600 700] % CID 1=500, CID 2=600, CID 3=700
    ///   100 200 300 % CIDs 100-200 all have width 300
    /// ]
    /// ```
    /// Inspect a Type0 font's `/Encoding` object and resolve the writing
    /// mode it implies, plus the encoding name preserved for diagnostics.
    ///
    /// Returns a pair `(name, wmode)` where:
    /// - `name` is the predefined-CMap name when `/Encoding` is a `/Name`
    ///   atom (`Identity-H`, `Identity-V`, `UniJIS-UTF16-V`, …) or the
    ///   embedded CMap stream's `/CMapName` value when `/Encoding` is a
    ///   stream/dict reference.
    /// - `wmode` is `1` when the resolved name ends in `-V` or equals the
    ///   bare legacy `V`, or when the embedded CMap stream contains a
    ///   `/WMode 1 def` directive. `0` otherwise (including unknown).
    ///
    /// The two signals are surfaced separately so callers can apply the
    /// precedence rules from ISO 32000-1 §9.7.5.4: an embedded CMap stream's
    /// explicit `/WMode` overrides what the name might suggest.
    pub(super) fn resolve_encoding_writing_mode(
        enc_obj: &Object,
        doc: &PdfDocument,
    ) -> (Option<String>, u8) {
        // Case 1: /Encoding is a /Name atom — predefined CMap name.
        if let Some(name) = enc_obj.as_name() {
            let wmode = wmode_from_predefined_cmap_name(name);
            return (Some(name.to_string()), wmode);
        }

        // Case 2: /Encoding is a stream/dict — embedded CMap. The dict may
        // expose a /CMapName and the stream body may carry /WMode N def.
        let dict = enc_obj.as_dict();
        let name = dict
            .and_then(|d| d.get("CMapName"))
            .and_then(|n| n.as_name())
            .map(|s| s.to_string());

        // Try to decode the CMap stream and scan for /WMode. We swallow
        // decode errors here — if the stream cannot be decoded, the existing
        // `parse_encoding` path will eventually log it; for wmode detection
        // we silently fall back to the name-based signal.
        let stream_wmode = match enc_obj.decode_stream_data() {
            Ok(bytes) => {
                let content = String::from_utf8_lossy(&bytes);
                crate::fonts::cmap::parse_wmode_directive_public(&content)
            }
            Err(_) => None,
        };
        let _ = doc; // doc reserved for future use (e.g. resolving /UseCMap refs).

        let name_wmode = name
            .as_deref()
            .map(wmode_from_predefined_cmap_name)
            .unwrap_or(0);
        let wmode = stream_wmode.unwrap_or(name_wmode);
        (name, wmode)
    }

    /// Parse `/DW2` from a CIDFont dictionary.
    ///
    /// Per ISO 32000-1 §9.7.4.3 the value is an array of two numbers:
    /// `[v_y_default w1y_default]`. Spec default when `/DW2` is absent is
    /// `[880 -1000]`. The default `v_x` is always `500` (half-em) — the spec
    /// does not provide a way to override it via `/DW2`.
    ///
    /// Returns the parsed defaults, or [`VerticalMetrics::SPEC_DEFAULT`] when
    /// `/DW2` is missing or malformed.
    pub(super) fn parse_dw2(cidfont_dict: &HashMap<String, Object>) -> VerticalMetrics {
        let Some(dw2_obj) = cidfont_dict.get("DW2") else {
            return VerticalMetrics::SPEC_DEFAULT;
        };
        let Some(arr) = dw2_obj.as_array() else {
            return VerticalMetrics::SPEC_DEFAULT;
        };
        if arr.len() < 2 {
            return VerticalMetrics::SPEC_DEFAULT;
        }
        let v_y = match &arr[0] {
            Object::Integer(i) => *i as f32,
            Object::Real(r) => *r as f32,
            _ => return VerticalMetrics::SPEC_DEFAULT,
        };
        let w1y = match &arr[1] {
            Object::Integer(i) => *i as f32,
            Object::Real(r) => *r as f32,
            _ => return VerticalMetrics::SPEC_DEFAULT,
        };
        VerticalMetrics {
            w1y,
            v_x: 500.0,
            v_y,
        }
    }

    /// Parse `/W2` (per-CID vertical metrics) from a CIDFont dictionary.
    ///
    /// Per ISO 32000-1 §9.7.4.3 the `/W2` array uses two forms, both of which
    /// may be intermixed within a single `/W2`:
    ///
    /// - Form A — explicit per-CID metrics:
    ///   `c [ w1y v_x v_y w1y v_x v_y … ]` — the inner array holds successive
    ///   `(w1y, v_x, v_y)` triples assigned to CIDs `c, c+1, c+2, …`.
    ///
    /// - Form B — range:
    ///   `c_first c_last w1y v_x v_y` — every CID in `c_first..=c_last`
    ///   shares the same `(w1y, v_x, v_y)`.
    ///
    /// Returns `None` when `/W2` is absent or empty, allowing callers to skip
    /// the HashMap allocation entirely on horizontal fonts.
    pub(super) fn parse_cid_vertical_metrics(
        cidfont_dict: &HashMap<String, Object>,
        base_font: &str,
    ) -> Option<HashMap<u16, VerticalMetrics>> {
        let w2_obj = cidfont_dict.get("W2")?;
        let w2_array = w2_obj.as_array()?;

        if w2_array.is_empty() {
            return None;
        }

        let mut metrics: HashMap<u16, VerticalMetrics> = HashMap::new();
        let mut i = 0;

        while i < w2_array.len() {
            let cid_start = match &w2_array[i] {
                Object::Integer(c) => *c as u16,
                _ => {
                    log::warn!(
                        "Font '{}': /W2 array element {} is not an integer, skipping",
                        base_font,
                        i
                    );
                    i += 1;
                    continue;
                }
            };
            i += 1;

            if i >= w2_array.len() {
                break;
            }

            match &w2_array[i] {
                Object::Array(triples) => {
                    // Form A: c [ w1y v_x v_y w1y v_x v_y … ]
                    // Walk the inner array in groups of three. A triple is
                    // atomic: if any of its three elements is non-numeric
                    // we drop the WHOLE triple (advance j+=3, emitted+=1)
                    // so the CID alignment of the rest of the inner array
                    // is preserved. The original implementation advanced
                    // j by 1 on a malformed element, which silently
                    // shifted every subsequent CID by one slot.
                    let mut j = 0;
                    let mut emitted: u32 = 0;
                    let read_num = |obj: &Object| -> Option<f32> {
                        match obj {
                            Object::Integer(v) => Some(*v as f32),
                            Object::Real(v) => Some(*v as f32),
                            _ => None,
                        }
                    };
                    while j + 2 < triples.len() {
                        let triple = (
                            read_num(&triples[j]),
                            read_num(&triples[j + 1]),
                            read_num(&triples[j + 2]),
                        );
                        // Compute CID with overflow detection BEFORE writing.
                        // saturating_add(emitted) would collapse every
                        // overflowing slot onto u16::MAX; instead we stop.
                        let Some(cid) = (cid_start as u32).checked_add(emitted) else {
                            log::warn!(
                                "Font '{}': /W2 Form A starting at CID {} overflowed u32 \
                                 at emitted offset {}; stopping",
                                base_font,
                                cid_start,
                                emitted
                            );
                            break;
                        };
                        if cid > u16::MAX as u32 {
                            log::warn!(
                                "Font '{}': /W2 Form A starting at CID {} would assign \
                                 beyond u16::MAX at emitted offset {}; stopping",
                                base_font,
                                cid_start,
                                emitted
                            );
                            break;
                        }
                        match triple {
                            (Some(w1y), Some(v_x), Some(v_y)) => {
                                metrics.insert(cid as u16, VerticalMetrics { w1y, v_x, v_y });
                            }
                            _ => {
                                log::warn!(
                                    "Font '{}': /W2 Form A triple starting at CID {} (offset \
                                     {}) is malformed; dropping it (keeping CID alignment)",
                                    base_font,
                                    cid_start,
                                    emitted
                                );
                            }
                        }
                        emitted += 1;
                        j += 3;
                    }
                    i += 1;
                }
                Object::Integer(cid_end_int) => {
                    // Form B: c_first c_last w1y v_x v_y
                    let cid_end = *cid_end_int as u16;
                    i += 1;
                    if i + 2 >= w2_array.len() {
                        log::warn!(
                            "Font '{}': /W2 range starting at CID {} truncated",
                            base_font,
                            cid_start
                        );
                        break;
                    }
                    let read = |obj: &Object| -> Option<f32> {
                        match obj {
                            Object::Integer(v) => Some(*v as f32),
                            Object::Real(v) => Some(*v as f32),
                            _ => None,
                        }
                    };
                    let Some(w1y) = read(&w2_array[i]) else {
                        i += 3;
                        continue;
                    };
                    let Some(v_x) = read(&w2_array[i + 1]) else {
                        i += 3;
                        continue;
                    };
                    let Some(v_y) = read(&w2_array[i + 2]) else {
                        i += 3;
                        continue;
                    };
                    i += 3;
                    let metric = VerticalMetrics { w1y, v_x, v_y };
                    for cid in cid_start..=cid_end {
                        metrics.insert(cid, metric);
                    }
                }
                _ => {
                    log::warn!(
                        "Font '{}': /W2 array has unexpected element type after CID {}",
                        base_font,
                        cid_start
                    );
                    i += 1;
                }
            }
        }

        if metrics.is_empty() {
            None
        } else {
            Some(metrics)
        }
    }

    pub(super) fn parse_cid_widths(
        cidfont_dict: &HashMap<String, Object>,
        base_font: &str,
    ) -> Option<HashMap<u16, f32>> {
        let w_obj = cidfont_dict.get("W")?;
        let w_array = w_obj.as_array()?;

        if w_array.is_empty() {
            return None;
        }

        let mut widths: HashMap<u16, f32> = HashMap::new();
        let mut i = 0;

        while i < w_array.len() {
            // First element must be a CID (integer)
            let cid_start = match &w_array[i] {
                Object::Integer(c) => *c as u16,
                _ => {
                    log::warn!(
                        "Font '{}': /W array element {} is not an integer, skipping",
                        base_font,
                        i
                    );
                    i += 1;
                    continue;
                }
            };
            i += 1;

            if i >= w_array.len() {
                break;
            }

            // Second element is either:
            // - An array of widths (format: c [w1 w2 ...])
            // - An integer CID end (format: cfirst clast w)
            match &w_array[i] {
                Object::Array(width_array) => {
                    // Format: c [w1 w2 ... wn]
                    for (j, width_obj) in width_array.iter().enumerate() {
                        let width = match width_obj {
                            Object::Integer(w) => *w as f32,
                            Object::Real(w) => *w as f32,
                            _ => continue,
                        };
                        let cid = cid_start.saturating_add(j as u16);
                        widths.insert(cid, width);
                    }
                    i += 1;
                }
                Object::Integer(cid_end) => {
                    // Format: cfirst clast w
                    let cid_end = *cid_end as u16;
                    i += 1;

                    if i >= w_array.len() {
                        log::warn!(
                            "Font '{}': /W array missing width for CID range {}-{}",
                            base_font,
                            cid_start,
                            cid_end
                        );
                        break;
                    }

                    let width = match &w_array[i] {
                        Object::Integer(w) => *w as f32,
                        Object::Real(w) => *w as f32,
                        _ => {
                            log::warn!(
                                "Font '{}': /W array has invalid width for CID range {}-{}",
                                base_font,
                                cid_start,
                                cid_end
                            );
                            i += 1;
                            continue;
                        }
                    };
                    i += 1;

                    // Apply width to all CIDs in range
                    for cid in cid_start..=cid_end {
                        widths.insert(cid, width);
                    }
                }
                _ => {
                    log::warn!(
                        "Font '{}': /W array has unexpected element type after CID {}",
                        base_font,
                        cid_start
                    );
                    i += 1;
                }
            }
        }

        if widths.is_empty() {
            None
        } else {
            Some(widths)
        }
    }

    /// Vertical advance and origin offset for a CID, in 1000ths-of-em.
    ///
    /// Lookup order:
    /// 1. Per-CID entry from `/W2` (if `cid_vertical_metrics` is populated).
    /// 2. `/DW2` defaults (`cid_default_vertical_metrics`).
    /// 3. Spec defaults from [`VerticalMetrics::SPEC_DEFAULT`] when the font
    ///    is not a CIDFont (e.g. simple Type1/TrueType): callers that
    ///    reach this with a non-Type0 font are degenerate, but returning
    ///    spec defaults is safe.
    ///
    /// This is the vertical counterpart to [`FontInfo::get_glyph_width`] and
    /// is read on the hot path of the renderer / extractor whenever
    /// `self.wmode == 1`.
    #[inline]
    pub fn get_vertical_metrics(&self, cid: u16) -> VerticalMetrics {
        if let Some(map) = &self.cid_vertical_metrics {
            if let Some(&m) = map.get(&cid) {
                return m;
            }
        }
        self.cid_default_vertical_metrics
    }
}
