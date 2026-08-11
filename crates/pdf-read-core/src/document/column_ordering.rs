use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// B1: merge a contiguous same-line run of spans that *crosses the page
    /// midline* into a single span, so the converter pipeline's column cut
    /// cannot shred a full-width line that the producer happened to draw as two
    /// adjacent show-strings. A generator may emit a full-width heading line
    /// above a two-column body as two fragments split mid-word at the gutter;
    /// each fragment's centre-x then falls in a different column, so the
    /// geometric reading order buckets them apart and the second half surfaces
    /// far away in the other column's stream. The plain-text path never hits
    /// this because it runs `merge_adjacent_spans` *before* column detection;
    /// this mirrors that for the md/html converter paths.
    ///
    /// Returns `Some(new spans)` only when at least one gutter-crossing run was
    /// merged, else `None` (the caller keeps the original spans, byte-identical).
    /// Tightly scoped: fires only on a multi-column page, and only merges a run
    /// whose member spans are contiguous (small inter-span gap, same font size)
    /// AND whose combined x-extent straddles the content midline — the unique
    /// signature of a full-width line drawn as multiple fragments. A normal
    /// per-column body run never crosses the midline (the gutter gap breaks the
    /// contiguity at the column edge), so those pages are untouched.
    pub(super) fn coalesce_gutter_crossing_runs(
        spans: &[crate::layout::TextSpan],
    ) -> Option<Vec<crate::layout::TextSpan>> {
        use crate::utils::safe_float_cmp;
        if spans.len() < 2 || !Self::is_multi_column_page(spans) {
            return None;
        }
        let finite = |s: &crate::layout::TextSpan| {
            s.bbox.x.is_finite() && s.bbox.y.is_finite() && s.bbox.width.is_finite()
        };
        let cmin = spans
            .iter()
            .filter(|s| finite(s) && !s.text.trim().is_empty())
            .map(|s| s.bbox.x)
            .fold(f32::INFINITY, f32::min);
        let cmax = spans
            .iter()
            .filter(|s| finite(s) && !s.text.trim().is_empty())
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        if !cmin.is_finite() || !cmax.is_finite() || cmax - cmin < 100.0 {
            return None;
        }
        let mid = (cmin + cmax) * 0.5;

        // Group original indices into visual lines by a font-relative Y band.
        let mut order: Vec<usize> = (0..spans.len()).filter(|&i| finite(&spans[i])).collect();
        order.sort_by(|&a, &b| {
            safe_float_cmp(spans[b].bbox.y, spans[a].bbox.y)
                .then_with(|| safe_float_cmp(spans[a].bbox.x, spans[b].bbox.x))
        });

        // index → action: a merged span replaces the run's first member; the
        // rest are dropped. Everything else is kept in its original position.
        let mut replacement: std::collections::HashMap<usize, crate::layout::TextSpan> =
            std::collections::HashMap::new();
        let mut dropped: std::collections::HashSet<usize> = std::collections::HashSet::new();

        let mut li = 0;
        while li < order.len() {
            let anchor_y = spans[order[li]].bbox.y;
            let mut lj = li + 1;
            while lj < order.len() {
                let fs = spans[order[li]]
                    .font_size
                    .max(spans[order[lj]].font_size)
                    .max(1.0);
                if (spans[order[lj]].bbox.y - anchor_y).abs() > 0.5 * fs {
                    break;
                }
                lj += 1;
            }
            // `order[li..lj]` is one visual line, already x-ascending.
            let line = &order[li..lj];
            let mut k = 0;
            while k < line.len() {
                // Extend a contiguous run: small inter-span gap + matching font size.
                let mut m = k + 1;
                while m < line.len() {
                    let prev = &spans[line[m - 1]];
                    let cur = &spans[line[m]];
                    let fs = prev.font_size.max(cur.font_size).max(1.0);
                    let gap = cur.bbox.x - (prev.bbox.x + prev.bbox.width);
                    // Keep a run UNIFORM in styling. `merge_same_line_run` takes
                    // the first span's style for the whole merged span, so a run
                    // that mixes weights/italics would silently erase an interior
                    // emphasis span (a lone bold word bracketed by body text loses
                    // its `**`). A genuine mid-word gutter split is always one
                    // word in one style, so requiring uniform weight+italic keeps
                    // that fix while never dropping emphasis.
                    if gap > 0.5 * fs
                        || (cur.font_size - prev.font_size).abs() > 0.1
                        || cur.font_weight != prev.font_weight
                        || cur.is_italic != prev.is_italic
                    {
                        break;
                    }
                    m += 1;
                }
                let run = &line[k..m];
                if run.len() >= 2 {
                    let run_min = spans[run[0]].bbox.x;
                    let last = &spans[run[run.len() - 1]];
                    let run_max = last.bbox.x + last.bbox.width;
                    // Only stitch a gutter-crossing run that actually carries a
                    // MID-WORD split straddling the midline: an adjacent pair
                    // joined by a hairline/negative gap whose two sides are both
                    // alphanumeric (the producer broke one word into two
                    // show-strings across the gutter). A legitimate full-width
                    // line drawn as several fragments breaks at WORD boundaries
                    // (space-width gaps) or non-letter glyph edges, so it never
                    // matches and stays byte-identical — this keeps the change
                    // scoped to the split-word case alone.
                    let has_midword_split_across_mid = run.windows(2).any(|w| {
                        let prev = &spans[w[0]];
                        let cur = &spans[w[1]];
                        let fs = prev.font_size.max(cur.font_size).max(1.0);
                        let gap = cur.bbox.x - (prev.bbox.x + prev.bbox.width);
                        let prev_tok = prev.text.split_whitespace().last().unwrap_or("");
                        let cur_tok = cur.text.split_whitespace().next().unwrap_or("");
                        // A genuine producer split breaks ONE word or number into two
                        // adjacent show-strings, leaving a tiny fragment on one side:
                        // a lone uppercase initial (a name abbreviation) or the two
                        // halves of a digit pair. On a tight multi-column page a font
                        // with over-estimated advance widths inflates each column
                        // line's bbox so a left-column line overruns the gutter and
                        // abuts the next column, making two DIFFERENT complete words
                        // look like a hairline split. Those join two complete tokens,
                        // so the split is real only when a boundary token is a lone
                        // uppercase letter or a two-digit number — never a 2-letter
                        // word, a complete word, or a bare figure/table digit. The gap
                        // can't separate the two cases (a genuine split overlaps as
                        // deeply as a column line), but a deep overlap (≥ half an em)
                        // is always a column abut.
                        let tok_is_fragment = |t: &str| {
                            let mut cs = t.chars();
                            match (cs.next(), cs.next(), cs.next()) {
                                // A lone uppercase letter — a name initial.
                                (Some(c), None, _) => c.is_alphabetic() && c.is_uppercase(),
                                // The two digits of a split number.
                                (Some(a), Some(b), None) => {
                                    a.is_ascii_digit() && b.is_ascii_digit()
                                }
                                _ => false,
                            }
                        };
                        gap >= -0.5 * fs
                            && gap <= 0.15 * fs
                            && prev.bbox.x < mid
                            && cur.bbox.x + cur.bbox.width > mid
                            && (tok_is_fragment(prev_tok) || tok_is_fragment(cur_tok))
                            && prev_tok
                                .chars()
                                .next_back()
                                .is_some_and(|c| c.is_alphanumeric())
                            && cur_tok.chars().next().is_some_and(|c| c.is_alphanumeric())
                    });
                    if run_min < mid && run_max > mid && has_midword_split_across_mid {
                        replacement.insert(run[0], Self::merge_same_line_run(spans, run));
                        for &idx in &run[1..] {
                            dropped.insert(idx);
                        }
                    }
                }
                k = m;
            }
            li = lj;
        }

        if replacement.is_empty() {
            return None;
        }
        // Rebuild in original order, substituting merged spans and dropping the
        // swallowed fragments; non-finite spans pass through untouched.
        let mut out = Vec::with_capacity(spans.len());
        for (i, s) in spans.iter().enumerate() {
            if dropped.contains(&i) {
                continue;
            }
            match replacement.remove(&i) {
                Some(merged) => out.push(merged),
                None => out.push(s.clone()),
            }
        }
        Some(out)
    }

    /// Merge a contiguous, x-ascending run of same-line spans (indices into
    /// `spans`) into one span: union the bounding box, take styling/MCID from the
    /// first member, and join the text with a single space wherever the
    /// inter-span gap is wide enough to be a word break. A negative / hairline
    /// gap is a mid-word fragment boundary and is concatenated directly, so the
    /// two halves of a word split across the gutter rejoin without a stray space.
    pub(super) fn merge_same_line_run(
        spans: &[crate::layout::TextSpan],
        run: &[usize],
    ) -> crate::layout::TextSpan {
        let mut merged = spans[run[0]].clone();
        let mut x_max = merged.bbox.x + merged.bbox.width;
        for &idx in &run[1..] {
            let s = &spans[idx];
            let fs = merged.font_size.max(s.font_size).max(1.0);
            let gap = s.bbox.x - x_max;
            if gap > 0.25 * fs
                && !merged.text.ends_with(' ')
                && !s.text.starts_with(' ')
                && !merged.text.is_empty()
            {
                merged.text.push(' ');
            }
            merged.text.push_str(&s.text);
            x_max = x_max.max(s.bbox.x + s.bbox.width);
            merged.bbox.y = merged.bbox.y.min(s.bbox.y);
        }
        merged.bbox.width = (x_max - merged.bbox.x).max(0.0);
        merged.char_widths = Vec::new();
        merged
    }

    /// If `spans` is a genuine two-column-prose page (#734), reorder them
    /// column-major with full-width band separation; otherwise a no-op. Shared
    /// by `extract_text`, `to_markdown`, and `to_html` so every flow agrees on
    /// the reading order of a two-column body. Returns `true` if a reorder was
    /// applied (the caller can then suppress geometric block re-ordering that
    /// would otherwise re-derive row-major order from positions).
    pub(super) fn reorder_two_column_prose(spans: &mut Vec<crate::layout::TextSpan>) -> bool {
        match Self::prose_two_column_gutter(spans) {
            Some(gutter_x) => {
                Self::reorder_column_major_with_bands(spans, gutter_x);
                true
            }
            None => false,
        }
    }

    /// Reorder a confirmed two-column-prose page **column-major with band
    /// separation** (#734). Walks rows top→bottom; a row containing a span that
    /// *crosses* the gutter is a full-width band (title, section heading,
    /// footer) and is emitted at its vertical position, between the column runs
    /// around it. Column runs are flushed left-column-then-right-column. This is
    /// the §14.8.3 layout model: full-width BLSEs interleave with columns by
    /// block position, so a mid-body heading is NOT split across the gutter.
    pub(super) fn reorder_column_major_with_bands(
        spans: &mut Vec<crate::layout::TextSpan>,
        gutter_x: f32,
    ) {
        use crate::layout::TextSpan;
        // A genuine full-width BAND (title/heading/footer that spans both
        // columns) extends meaningfully on BOTH sides of the gutter. Require an
        // 8pt overhang each side so a column item whose bbox merely *clips* the
        // gutter by a few points — e.g. a hanging reference number ("42.") at the
        // right column's left edge, or a wrapped line reaching just past the
        // gutter — is NOT mistaken for a band and pulled out of its column.
        let crosses =
            |s: &TextSpan| s.bbox.x < gutter_x - 8.0 && s.bbox.x + s.bbox.width > gutter_x + 8.0;
        let mut src = std::mem::take(spans);
        // Top→bottom, then left→right within a row. Quantize Y to the row band
        // (`row_aware_span_cmp`) so sub-point baseline jitter between spans on
        // the SAME visual line (font-metric rounding, a superscript citation's
        // slightly different Y) cannot invert their X order: a 0.001pt Y
        // difference under a raw `safe_float_cmp` would sort a mid-line span
        // ahead of the line's left-edge span, scrambling the line
        // (PMC8129076 "phase and amplitude of clock-controlled genes83. Thus,
        // it is clear" — the "83" citation lifts ". Thus" onto a 0.001pt-higher
        // baseline). The downstream row grouping already uses a 3pt tolerance;
        // matching it here keeps the two consistent.
        src.sort_by(|a, b| {
            crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
        });
        let mut out: Vec<TextSpan> = Vec::with_capacity(src.len());
        let mut col_buf: Vec<TextSpan> = Vec::new();
        let flush = |buf: &mut Vec<TextSpan>, out: &mut Vec<TextSpan>| {
            if buf.is_empty() {
                return;
            }
            // A full line-height, used to decide when a block sits clearly
            // *below* the opposite column rather than beside it.
            let mut heights: Vec<f32> = buf.iter().map(|s| s.bbox.height).collect();
            heights.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
            let line_h = heights
                .get(heights.len() / 2)
                .copied()
                .unwrap_or(10.0)
                .max(1.0);
            // Left column (centre < gutter) by y, then right column by y.
            let (mut left, mut right): (Vec<TextSpan>, Vec<TextSpan>) = std::mem::take(buf)
                .into_iter()
                .partition(|s| s.bbox.x + s.bbox.width * 0.5 < gutter_x);
            // Row-banded (Y quantized to ROW_BAND_TOLERANCE_PT) so sub-point
            // baseline jitter on a single visual line cannot invert the X order
            // within that line; see the pre-sort note above.
            let by_yx = |a: &TextSpan, b: &TextSpan| {
                crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
            };
            // Trailing-block peel: a block lying a full line-height BELOW the
            // entire opposite column is a bottom-spanning block (e.g. a
            // bottom-left References section), not a parallel column member, so
            // it must read AFTER both columns at its own y — not within its
            // column partition (which would print it before the whole opposite
            // column). oxide bbox.y is top-up (higher y = higher on page), so
            // "below" = smaller y. Only fires when the opposite column has real
            // content (>=2 spans) and the block clears its bottom by a line, so
            // balanced 2-col bodies (columns ending at ~equal y) are untouched.
            let bottom_y =
                |v: &[TextSpan]| v.iter().map(|s| s.bbox.y).fold(f32::INFINITY, f32::min);
            let right_bottom = bottom_y(&right);
            let left_bottom = bottom_y(&left);
            let mut trailing: Vec<TextSpan> = Vec::new();
            if right.len() >= 2 {
                left.retain(|s| {
                    let below = s.bbox.y < right_bottom - line_h;
                    if below {
                        trailing.push(s.clone());
                    }
                    !below
                });
            }
            if left.len() >= 2 {
                right.retain(|s| {
                    let below = s.bbox.y < left_bottom - line_h;
                    if below {
                        trailing.push(s.clone());
                    }
                    !below
                });
            }
            left.sort_by(by_yx);
            right.sort_by(by_yx);
            trailing.sort_by(by_yx);
            out.append(&mut left);
            out.append(&mut right);
            out.append(&mut trailing);
        };
        let mut i = 0;
        while i < src.len() {
            let y0 = src[i].bbox.y;
            let mut row: Vec<TextSpan> = Vec::new();
            while i < src.len() && (src[i].bbox.y - y0).abs() <= 3.0 {
                row.push(src[i].clone());
                i += 1;
            }
            if row.iter().any(crosses) {
                // Full-width band row: flush the columns above it, then emit the
                // band whole (left→right), keeping it out of the column stream.
                flush(&mut col_buf, &mut out);
                row.sort_by(|a, b| crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x));
                out.append(&mut row);
            } else {
                col_buf.append(&mut row);
            }
        }
        flush(&mut col_buf, &mut out);
        *spans = out;
    }

    /// True when the page's multi-column geometric signal is explained by a
    /// detected TABLE rather than a genuine two-column text body.
    ///
    /// Used by the geometric reading-order dispatch: when the genuine
    /// two-column branches (topological / prose-gutter / classifier) all
    /// declined yet `is_multi_column_page` is still true, the page is either a
    /// single-column body with a data table (whose column-aligned cells trip the
    /// detector) or a two-column body the column branches missed. We only want
    /// to override the multi-column gate (and apply the row-aware band sort) in
    /// the FIRST case.
    ///
    /// Discriminator: cluster the per-line left edges of the spans OUTSIDE the
    /// detected table regions (the surrounding prose). A single-column body has
    /// ONE dominant left edge there; a genuine two-column body has two. We
    /// require a strong single dominant left-edge cluster (≥ 70% of non-table
    /// lines), so a two-column page — whose non-table prose still splits into two
    /// left-edge clusters — is rejected. Spans inside the table contribute their
    /// own column-aligned left edges and are deliberately excluded.
    pub(super) fn multicol_signal_is_tabular(
        spans: &[crate::layout::TextSpan],
        tables: &[crate::structure::table_extractor::Table],
    ) -> bool {
        // Expand each table bbox slightly upward to absorb header rows the
        // spatial extractor often leaves just above the captured cell grid.
        let in_table = |s: &crate::layout::TextSpan| -> bool {
            let cx = s.bbox.x + s.bbox.width * 0.5;
            let cy = s.bbox.y + s.bbox.height * 0.5;
            tables.iter().any(|t| {
                t.bbox.is_some_and(|b| {
                    cx >= b.x - 2.0
                        && cx <= b.x + b.width + 2.0
                        && cy >= b.y - 2.0
                        && cy <= b.y + b.height + 14.0
                })
            })
        };
        let outside: Vec<&crate::layout::TextSpan> = spans
            .iter()
            .filter(|s| !s.text.trim().is_empty() && s.bbox.width > 0.0 && !in_table(s))
            .collect();
        if outside.len() < 8 {
            // The table is essentially the whole page; the row-aware band sort
            // linearises it correctly, so treat the signal as tabular.
            return true;
        }
        // Per-line (Y-band) minimum left edge.
        let mut by_band: std::collections::BTreeMap<i32, f32> = std::collections::BTreeMap::new();
        for s in &outside {
            let band = (s.bbox.y / 2.0).round() as i32;
            let e = by_band.entry(band).or_insert(f32::INFINITY);
            *e = e.min(s.bbox.x);
        }
        let mut lefts: Vec<f32> = by_band
            .values()
            .copied()
            .filter(|v| v.is_finite())
            .collect();
        if lefts.len() < 6 {
            return true;
        }
        lefts.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        // Cluster left edges with a 12pt gap (≈ one indent); the largest cluster
        // is the body's left margin.
        let mut clusters: Vec<usize> = Vec::new();
        let mut run = 1usize;
        let mut prev = lefts[0];
        for &v in &lefts[1..] {
            if v - prev > 12.0 {
                clusters.push(run);
                run = 0;
            }
            run += 1;
            prev = v;
        }
        clusters.push(run);
        let total = lefts.len();
        let top = *clusters.iter().max().unwrap_or(&0);
        // Strong single dominant left edge ⇒ single-column prose ⇒ the
        // multi-column signal came from the table.
        top as f32 >= 0.70 * total as f32
    }

    pub(super) fn is_multi_column_page(spans: &[crate::layout::TextSpan]) -> bool {
        // Clean-gutter detector (handles short pages the histogram gates below
        // reject for lack of spans). A genuine empty vertical channel that no
        // span crosses, with multi-line content of overlapping vertical extent
        // on both sides, is the unambiguous geometric signature of side-by-side
        // columns — recoverable for untagged pages only from layout (XY-Cut,
        // ISO 32000-1 §9.4, since there is no logical-structure hint).
        if Self::has_clean_column_gutter(spans) {
            return true;
        }

        if spans.len() < 12 {
            return false; // too few to confidently split into columns
        }

        // Primary detector: line-start-X bimodality.
        //
        // The span-center histogram further down is noisy for word-level
        // spans (every X position has many word starts on multi-word
        // body-text lines). The reliable signal is the X position at
        // which each *line* begins — a two-column body has a strong
        // peak at the left-column-start X plus a strong peak at the
        // right-column-start X, with a clear empty gutter between
        // them. We cluster spans into lines by Y (1pt tolerance), pick
        // the leftmost X per line, and look for ≥ 2 peaks separated by
        // a gutter of ≥ 30pt with zero line-starts in it.
        if Self::has_bimodal_line_starts(spans) {
            return true;
        }

        let mut x_centers: Vec<f32> = spans
            .iter()
            .map(|s| s.bbox.x + s.bbox.width * 0.5)
            .collect();
        x_centers.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));

        // Degenerate CTM guard: drop centers more than MAX_EXTENT from the
        // median so a rogue span ~1e16 doesn't explode the histogram.
        const MAX_EXTENT_FROM_MEDIAN: f32 = 5_000.0;
        let median = x_centers[x_centers.len() / 2];
        x_centers.retain(|c| (*c - median).abs() <= MAX_EXTENT_FROM_MEDIAN);
        if x_centers.len() < 12 {
            return false;
        }

        let min = *x_centers.first().unwrap();
        let max = *x_centers.last().unwrap();
        let width = max - min;
        if width < 100.0 {
            return false; // spans cluster in a single vertical line — not columns
        }

        // Bin into 40 buckets; find peaks (≥ mean × 1.5) separated by at
        // least one empty bucket.
        const BUCKETS: usize = 40;
        let bucket_width = width / BUCKETS as f32;
        if bucket_width <= 0.0 {
            return false;
        }
        let mut hist = [0usize; BUCKETS];
        for c in &x_centers {
            let idx = (((c - min) / bucket_width) as usize).min(BUCKETS - 1);
            hist[idx] += 1;
        }

        let total: usize = hist.iter().sum();
        let mean = total as f32 / BUCKETS as f32;
        let threshold = (mean * 1.5).max(3.0);

        let mut peaks = 0usize;
        let mut in_peak = false;
        for &count in &hist {
            if count as f32 >= threshold {
                if !in_peak {
                    peaks += 1;
                    in_peak = true;
                }
            } else if count == 0 {
                in_peak = false;
            }
        }

        if peaks < 2 {
            return false;
        }

        // Confirmation: the peaks must have vertical overlap. If one "column"
        // is a footer and the other is the body, they don't interact — row-
        // aware is fine. Split spans into left-half vs right-half and check
        // their Y ranges overlap.
        let mid_x = (min + max) / 2.0;
        let mut left_y_min = f32::INFINITY;
        let mut left_y_max = f32::NEG_INFINITY;
        let mut right_y_min = f32::INFINITY;
        let mut right_y_max = f32::NEG_INFINITY;
        for s in spans {
            let cx = s.bbox.x + s.bbox.width * 0.5;
            if (cx - median).abs() > MAX_EXTENT_FROM_MEDIAN {
                continue;
            }
            let y_top = s.bbox.y + s.bbox.height;
            if cx < mid_x {
                left_y_min = left_y_min.min(s.bbox.y);
                left_y_max = left_y_max.max(y_top);
            } else {
                right_y_min = right_y_min.min(s.bbox.y);
                right_y_max = right_y_max.max(y_top);
            }
        }
        let left_span = (left_y_max - left_y_min).max(0.0);
        let right_span = (right_y_max - right_y_min).max(0.0);
        let overlap = left_y_max.min(right_y_max) - left_y_min.max(right_y_min);
        let min_span = left_span.min(right_span);
        if !(min_span > 0.0 && overlap > 0.5 * min_span) {
            return false;
        }

        // Require each half to contain enough spans to represent genuine body
        // text columns. Copyright pages, title pages, and other sparse layouts
        // can produce two X-center peaks with only 2–7 spans per "column" —
        // these are not true multi-column body text.
        let left_count = spans
            .iter()
            .filter(|s| {
                let cx = s.bbox.x + s.bbox.width * 0.5;
                (cx - median).abs() <= MAX_EXTENT_FROM_MEDIAN && cx < mid_x
            })
            .count();
        let right_count = spans.len() - left_count;
        if left_count.min(right_count) < 15 {
            return false;
        }

        // Font-aware column-shape gate.
        //
        // Real two-column body text has tight column-edge alignment:
        // most spans on each side share one dominant X position
        // (the column start), with a handful of indented or
        // section-header outliers. Scattered-fragment layouts spread
        // their spans evenly across many X positions on each side.
        //
        // Measure the fraction of side-spans that fall into the
        // largest X-cluster (cluster gap = `dominant_em`). Body text
        // typically scores ≥ 0.5; scattered layouts score < 0.4.
        // Reject pages where either side fails the threshold so
        // XY-cut doesn't mis-route scattered content as multi-column.
        let stats = crate::layout::PageFontStats::from_spans(spans);
        let cluster_gap = stats.dominant_em.max(4.0);
        let dominant_cluster_fraction = |take: &dyn Fn(f32) -> bool| -> f32 {
            let mut xs: Vec<f32> = spans
                .iter()
                .filter(|s| {
                    let cx = s.bbox.x + s.bbox.width * 0.5;
                    (cx - median).abs() <= MAX_EXTENT_FROM_MEDIAN && take(cx)
                })
                .map(|s| s.bbox.x)
                .collect();
            let total = xs.len();
            if total == 0 {
                return 0.0;
            }
            xs.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
            let mut best = 1usize;
            let mut current = 1usize;
            let mut last = xs[0];
            for &x in &xs[1..] {
                if x - last <= cluster_gap {
                    current += 1;
                    if current > best {
                        best = current;
                    }
                } else {
                    current = 1;
                }
                last = x;
            }
            best as f32 / total as f32
        };
        const MIN_DOMINANT_FRACTION: f32 = 0.5;
        let left_frac = dominant_cluster_fraction(&|cx| cx < mid_x);
        let right_frac = dominant_cluster_fraction(&|cx| cx >= mid_x);
        if left_frac >= MIN_DOMINANT_FRACTION && right_frac >= MIN_DOMINANT_FRACTION {
            return true;
        }

        // Additive accept path (no change to the gate above): shared-baseline
        // two-column bodies — academic references / bibliographies — read
        // left+right on the SAME Y line, so the row-aware sort interleaves
        // them. Their word-granular left edges scatter, so the dominant-
        // cluster gate above misses them. But they exhibit ONE persistent
        // vertical gutter corridor (the signal poppler/MuPDF use, independent
        // of line length). Detect it via within-line gap projection, prose-
        // guarded so numeric / short-cell tables — which also reach here —
        // stay on the row-aware path. See #607.
        Self::has_persistent_gutter_corridor(spans, median, MAX_EXTENT_FROM_MEDIAN)
    }
}
