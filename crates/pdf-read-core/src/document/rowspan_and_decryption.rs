use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Promote labels in rowspan-sparse columns so they sort at the top
    /// of their data-row block instead of landing mid-group.
    ///
    /// A "label" here is a span in an X-cluster that contains far fewer
    /// spans than the most populous X-cluster (i.e., it spans multiple
    /// rows of the adjacent data column). Labels are typically vertically
    /// centred in their block, so a strict Y sort places them between
    /// the rows they describe. This post-processor detects the pattern
    /// and rewrites each label's effective sort Y to sit just above the
    /// topmost data row it visually covers.
    ///
    /// Data rows are partitioned between adjacent labels at the midpoint
    /// of their Y coordinates (nearest-label assignment). The topmost
    /// data row in a label's partition becomes the anchor for promotion.
    ///
    /// Nothing is mutated if there are no sparse columns or not enough
    /// data rows to confidently infer row-grouping (min 6 rows in the
    /// dense reference column).
    /// Identify span indices that look like multi-row-spanning labels —
    /// sparse-X-column spans whose Y values sit inside the data Y range
    /// of the dense columns on the page. These are the same spans that
    /// `reorder_rowspan_labels` would promote to the top of their row
    /// block, except this function returns them **before** the spatial
    /// table detector's retain filter has a chance to drop them from
    /// the flow span list.
    ///
    /// The retain filter in `extract_text_with_options` removes every
    /// span whose bbox is contained in a detected table's bbox. On CJK
    /// reference-data PDFs the test-name label column is
    /// narrow and vertically centred within each multi-row data block,
    /// so its spans are inside the table bbox and would be dropped
    /// without replacement — the spatial table extractor does not emit
    /// these labels as `TableCell`s either. Preserving the identified
    /// labels through the retain filter lets `reorder_rowspan_labels`
    /// promote them to their proper reading-order position alongside
    /// the surviving flow spans.
    ///
    /// Returns a `HashSet` of indices into the provided `spans` slice.
    /// Callers must use the returned indices **before** any reordering
    /// or retention mutates the slice.
    pub(crate) fn identify_multi_row_labels(
        spans: &[crate::layout::TextSpan],
    ) -> std::collections::HashSet<usize> {
        use std::collections::{BTreeSet, HashMap as StdHashMap, HashSet};

        let mut out: HashSet<usize> = HashSet::new();
        if spans.len() < 10 {
            return out;
        }

        // Cluster by X proximity (15pt gap threshold) — same heuristic
        // as `reorder_rowspan_labels`.
        let mut by_x: Vec<usize> = (0..spans.len()).collect();
        by_x.sort_by(|&a, &b| crate::utils::safe_float_cmp(spans[a].bbox.x, spans[b].bbox.x));
        const X_GAP: f32 = 15.0;
        let mut columns: Vec<Vec<usize>> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        let mut last_x = f32::NEG_INFINITY;
        for &idx in &by_x {
            let x = spans[idx].bbox.x;
            if !cur.is_empty() && x - last_x > X_GAP {
                columns.push(std::mem::take(&mut cur));
            }
            cur.push(idx);
            last_x = x;
        }
        if !cur.is_empty() {
            columns.push(cur);
        }
        if columns.len() < 2 {
            return out;
        }

        let max_count = columns.iter().map(|c| c.len()).max().unwrap_or(0);
        if max_count < 6 {
            return out;
        }

        // Sort columns by span count descending to pick the dense clusters.
        let mut col_order: Vec<usize> = (0..columns.len()).collect();
        col_order.sort_by(|&a, &b| columns[b].len().cmp(&columns[a].len()));
        let dense_cols_count = columns.iter().filter(|c| c.len() * 2 > max_count).count();

        let band_of = |y: f32| (y / crate::utils::ROW_BAND_TOLERANCE_PT).round() as i32;
        let data_bands: BTreeSet<i32> = if dense_cols_count >= 3 {
            let top: Vec<&Vec<usize>> = col_order.iter().take(3).map(|&i| &columns[i]).collect();
            let mut support: StdHashMap<i32, usize> = StdHashMap::new();
            for col in &top {
                let bands: HashSet<i32> = col.iter().map(|&i| band_of(spans[i].bbox.y)).collect();
                for b in bands {
                    *support.entry(b).or_insert(0) += 1;
                }
            }
            support
                .into_iter()
                .filter(|(_, c)| *c >= 3)
                .map(|(b, _)| b)
                .collect()
        } else if dense_cols_count == 2 {
            let a: HashSet<i32> = columns[col_order[0]]
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect();
            let b: HashSet<i32> = columns[col_order[1]]
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect();
            a.intersection(&b).copied().collect()
        } else {
            columns[col_order[0]]
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect()
        };

        if data_bands.len() < 4 {
            return out;
        }

        let band_pt = crate::utils::ROW_BAND_TOLERANCE_PT;
        let data_top = (*data_bands.iter().next_back().unwrap() as f32) * band_pt + band_pt / 2.0;
        let data_bot = (*data_bands.iter().next().unwrap() as f32) * band_pt - band_pt / 2.0;

        // Collect sparse-column spans that sit inside the data Y range
        // and belong to a column with >= 2 members in that range.
        for col in &columns {
            if col.len() < 2 || col.len() * 2 >= max_count {
                continue;
            }
            let in_data: Vec<usize> = col
                .iter()
                .copied()
                .filter(|&i| {
                    let y = spans[i].bbox.y;
                    y > data_bot && y < data_top
                })
                .collect();
            if in_data.len() >= 2 {
                out.extend(in_data);
            }
        }

        out
    }

    pub(crate) fn reorder_rowspan_labels(spans: &mut Vec<crate::layout::TextSpan>) {
        use std::collections::HashMap;

        if spans.len() < 10 {
            return;
        }

        // Cluster by X proximity (15pt gap threshold). Walk spans ordered
        // by left edge; start a new cluster whenever the gap exceeds the
        // threshold.
        let mut by_x: Vec<usize> = (0..spans.len()).collect();
        by_x.sort_by(|&a, &b| crate::utils::safe_float_cmp(spans[a].bbox.x, spans[b].bbox.x));
        const X_GAP: f32 = 15.0;
        let mut columns: Vec<Vec<usize>> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        let mut last_x = f32::NEG_INFINITY;
        for &idx in &by_x {
            let x = spans[idx].bbox.x;
            if !cur.is_empty() && x - last_x > X_GAP {
                columns.push(std::mem::take(&mut cur));
            }
            cur.push(idx);
            last_x = x;
        }
        if !cur.is_empty() {
            columns.push(cur);
        }
        if columns.len() < 2 {
            return;
        }

        // Max column size is our reference for "dense".
        let max_count = columns.iter().map(|c| c.len()).max().unwrap_or(0);
        if max_count < 6 {
            return;
        }

        // Sort columns by span count descending so we can pick the top
        // dense cluster for anchor detection.
        let mut col_order: Vec<usize> = (0..columns.len()).collect();
        col_order.sort_by(|&a, &b| columns[b].len().cmp(&columns[a].len()));

        // A column is "dense" when it holds a strict majority of the
        // most populous column's spans. Pages with multiple dense data
        // columns (three or more) let us derive the data-row range by
        // intersecting their Y bands — headers and sub-headers populate
        // only a subset of columns at their Y and fall out.
        let dense_cols_count = columns.iter().filter(|c| c.len() * 2 > max_count).count();

        // Most populous column, used for anchor Y lookups regardless.
        let dense_col = &columns[col_order[0]];
        let mut dense_ys: Vec<f32> = dense_col.iter().map(|&i| spans[i].bbox.y).collect();
        dense_ys.sort_by(|a, b| crate::utils::safe_float_cmp(*b, *a));

        // Compute the set of Y bands that count as "data". When several
        // dense columns are available, require a band to have support in
        // the top three; otherwise fall back to the single dense column's
        // own Y values.
        let band_of = |y: f32| (y / crate::utils::ROW_BAND_TOLERANCE_PT).round() as i32;
        use std::collections::{BTreeSet, HashMap as StdHashMap, HashSet};

        let data_bands: BTreeSet<i32> = if dense_cols_count >= 3 {
            let top: Vec<&Vec<usize>> = col_order.iter().take(3).map(|&i| &columns[i]).collect();
            let mut support: StdHashMap<i32, usize> = StdHashMap::new();
            for col in &top {
                let bands: HashSet<i32> = col.iter().map(|&i| band_of(spans[i].bbox.y)).collect();
                for b in bands {
                    *support.entry(b).or_insert(0) += 1;
                }
            }
            support
                .into_iter()
                .filter(|(_, c)| *c >= 3)
                .map(|(b, _)| b)
                .collect()
        } else if dense_cols_count == 2 {
            let a: HashSet<i32> = columns[col_order[0]]
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect();
            let b: HashSet<i32> = columns[col_order[1]]
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect();
            a.intersection(&b).copied().collect()
        } else {
            dense_col
                .iter()
                .map(|&i| band_of(spans[i].bbox.y))
                .collect()
        };

        if data_bands.len() < 4 {
            return;
        }
        let band_pt = crate::utils::ROW_BAND_TOLERANCE_PT;
        let data_top = (*data_bands.iter().next_back().unwrap() as f32) * band_pt + band_pt / 2.0;
        let data_bot = (*data_bands.iter().next().unwrap() as f32) * band_pt - band_pt / 2.0;

        // Y-bands occupied by the dense column. Genuine rowspan labels are
        // vertically centred *between* data rows, so their Y-band must NOT
        // appear in this set. Spans whose Y aligns with the dense column are
        // line-continuation text on the same logical line, not labels.
        let dense_bands: HashSet<i32> = dense_col
            .iter()
            .map(|&i| band_of(spans[i].bbox.y))
            .collect();

        // Numbered reference/bibliography lists render the leading marker
        // ("1.", "2.", "3.") in a narrow column to the left of the body
        // text. That marker column is sparse (one per entry) and its markers
        // sit between body rows, so they look exactly like rowspan labels to
        // the heuristic below — but they are NOT: each number belongs to its
        // own entry and promoting them scrambles the reference order. Detect
        // the pattern (>=3 numbered markers sharing a tight left-edge cluster
        // and spread down >=3 distinct rows = a vertical numbered list) and
        // exclude those markers from label promotion.
        let is_numbered_marker = |i: usize| -> bool {
            let t = spans[i].text.trim_start();
            let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
            (1..=3).contains(&digits) && t[digits..].starts_with(['.', ')'])
        };
        let numbered_excluded: HashSet<usize> = {
            let markers: Vec<usize> = (0..spans.len())
                .filter(|&i| is_numbered_marker(i))
                .collect();
            if markers.len() >= 3 {
                let mut xs: Vec<f32> = markers.iter().map(|&i| spans[i].bbox.x).collect();
                xs.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
                let median_x = xs[xs.len() / 2];
                let cluster: Vec<usize> = markers
                    .iter()
                    .copied()
                    .filter(|&i| (spans[i].bbox.x - median_x).abs() <= 6.0)
                    .collect();
                let rows: HashSet<i32> =
                    cluster.iter().map(|&i| band_of(spans[i].bbox.y)).collect();
                if cluster.len() >= 3 && rows.len() >= 3 {
                    cluster.into_iter().collect()
                } else {
                    HashSet::new()
                }
            } else {
                HashSet::new()
            }
        };

        // Collect "label" candidates: spans that sit in a "sparse"
        // column — one that holds meaningfully fewer spans than the
        // most populous column. A candidate only qualifies when it
        // sits strictly inside the data Y range AND the sparse column
        // it belongs to has at least two entries inside that range —
        // single-span sparse cells are almost always stray annotations,
        // not labels.
        let mut labels: Vec<usize> = Vec::new();
        for col in &columns {
            if col.len() < 2 || col.len() * 2 >= max_count {
                continue;
            }
            let in_data: Vec<usize> = col
                .iter()
                .copied()
                .filter(|&i| {
                    let y = spans[i].bbox.y;
                    // Exclude spans on the same Y-band as the dense column:
                    // those are line-continuation text, not rowspan labels.
                    // Also exclude numbered-list markers (reference numbers),
                    // which would otherwise be hoisted out of reading order.
                    y > data_bot
                        && y < data_top
                        && !dense_bands.contains(&band_of(y))
                        && !numbered_excluded.contains(&i)
                })
                .collect();
            if in_data.len() >= 2 {
                labels.extend(in_data);
            }
        }
        if labels.is_empty() {
            return;
        }
        labels.sort_by(|&a, &b| crate::utils::safe_float_cmp(spans[b].bbox.y, spans[a].bbox.y));

        // Labels that sit at near-identical Y values almost always
        // annotate the same logical row block (e.g. a test-name in the
        // "name" column alongside a unit "×10⁹/L" in the "unit" column,
        // both vertically centred in the same 6-row group). Cluster
        // labels by Y proximity so each logical block is promoted as a
        // unit.
        const CLUSTER_GAP: f32 = 10.0;
        let mut clusters: Vec<Vec<usize>> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        let mut last_y = f32::NAN;
        for &idx in &labels {
            let y = spans[idx].bbox.y;
            if !cur.is_empty() && (last_y - y).abs() > CLUSTER_GAP {
                clusters.push(std::mem::take(&mut cur));
            }
            cur.push(idx);
            last_y = y;
        }
        if !cur.is_empty() {
            clusters.push(cur);
        }
        let cluster_ys: Vec<f32> = clusters
            .iter()
            .map(|c| c.iter().map(|&i| spans[i].bbox.y).sum::<f32>() / c.len() as f32)
            .collect();

        // For each cluster, compute the midpoint partition boundaries
        // against its immediate neighbour clusters and find the topmost
        // dense-column Y that falls inside the partition. Promote every
        // member of the cluster to the same anchor so they sort together
        // at the top of their row block.
        let mut promoted: HashMap<usize, f32> = HashMap::new();
        for (k, cluster) in clusters.iter().enumerate() {
            let c_y = cluster_ys[k];
            let upper = if k > 0 {
                (cluster_ys[k - 1] + c_y) / 2.0
            } else {
                f32::INFINITY
            };
            let lower = if k + 1 < clusters.len() {
                (c_y + cluster_ys[k + 1]) / 2.0
            } else {
                f32::NEG_INFINITY
            };
            let upper_clamped = upper.min(data_top);
            let lower_clamped = lower.max(data_bot - 1.0);
            let mut anchor = f32::NEG_INFINITY;
            for &y in &dense_ys {
                if y <= upper_clamped && y > lower_clamped && y > anchor {
                    anchor = y;
                }
            }
            if anchor.is_finite() {
                for &i in cluster {
                    promoted.insert(i, anchor + 1.0);
                }
            }
        }
        if promoted.is_empty() {
            return;
        }

        // Re-sort spans using the promoted Ys for labels and actual Ys
        // for everything else. Keep the row-aware comparator so the
        // ordering stays consistent with the rest of the pipeline.
        let mut order: Vec<usize> = (0..spans.len()).collect();
        order.sort_by(|&a, &b| {
            let ya = promoted.get(&a).copied().unwrap_or(spans[a].bbox.y);
            let yb = promoted.get(&b).copied().unwrap_or(spans[b].bbox.y);
            crate::utils::row_aware_span_cmp(ya, spans[a].bbox.x, yb, spans[b].bbox.x)
        });
        let reordered: Vec<crate::layout::TextSpan> =
            order.into_iter().map(|i| spans[i].clone()).collect();
        *spans = reordered;
    }

    /// Recursively decrypt every `Object::String` inside `obj` using the
    /// per-object key derived from `obj_num`/`gen_num`. Streams are left
    /// untouched — they are decrypted lazily at read time through
    /// `decode_stream_with_encryption`. The `/Encrypt` dictionary itself
    /// must never be passed to this function; its strings are key material,
    /// not ciphertext.
    ///
    /// Per ISO 32000-1:2008 §7.6.2, strings inside encrypted-document
    /// objects are individually encrypted with the standard encryption
    /// algorithm. Parsed string tokens hold raw ciphertext and must be
    /// decrypted before downstream consumers (widget text, form field
    /// values, outlines, document info) can read them.
    pub(super) fn decrypt_strings_in_object(
        handler: &EncryptionHandler,
        obj: &mut Object,
        obj_num: u32,
        gen_num: u32,
    ) {
        match obj {
            Object::String(bytes) => match handler.decrypt_string(bytes, obj_num, gen_num) {
                Ok(decrypted) => *bytes = decrypted,
                Err(e) => {
                    log::debug!(
                        "String decryption failed for object {} {}: {}",
                        obj_num,
                        gen_num,
                        e
                    );
                }
            },
            Object::Array(items) => {
                for item in items {
                    Self::decrypt_strings_in_object(handler, item, obj_num, gen_num);
                }
            }
            Object::Dictionary(dict) => {
                for value in dict.values_mut() {
                    Self::decrypt_strings_in_object(handler, value, obj_num, gen_num);
                }
            }
            Object::Stream { dict, .. } => {
                // Stream *data* is decrypted separately in
                // `decode_stream_with_encryption`. Its dict may still
                // contain encrypted strings (e.g., /Metadata).
                for value in dict.values_mut() {
                    Self::decrypt_strings_in_object(handler, value, obj_num, gen_num);
                }
            }
            _ => {}
        }
    }
}
