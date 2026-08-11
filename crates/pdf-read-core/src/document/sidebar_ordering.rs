use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Detect a single persistent vertical gutter corridor across the page —
    /// the geometric fingerprint of a two-column prose body whose columns
    /// share Y baselines (so `has_bimodal_line_starts` and the dominant-
    /// cluster gate both miss it). Mirrors `detect_narrow_gutter_prose`
    /// (`src/pipeline/reading_order/xycut.rs`) at the document-routing layer.
    ///
    /// Table-safe by construction (#536). Long-line bodies
    /// (`mean non-whitespace chars per line > 20`) keep the original
    /// concentration / coverage / centre accept path. Short-line bodies
    /// (verse / lexicon editions) are admitted only under stricter,
    /// length-independent guards a numeric / short-cell table cannot satisfy:
    /// higher concentration and coverage, left/right column char-mass balance,
    /// and a grid-row signal (a multi-cell table has ≥ 2 wide gaps on most
    /// rows; a two-column body has one gutter). Full-width display-math /
    /// heading rows are excluded from the gutter-coverage denominator so a
    /// minority of them does not veto an otherwise two-column page.
    pub(super) fn has_persistent_gutter_corridor(
        spans: &[crate::layout::TextSpan],
        median: f32,
        max_extent: f32,
    ) -> bool {
        // Group spans into lines by rounded Y baseline; carry left/right
        // extents for gap projection and char count for the prose guard.
        let mut lines: std::collections::BTreeMap<i32, (Vec<(f32, f32)>, usize)> =
            std::collections::BTreeMap::new();
        let mut x_min = f32::MAX;
        let mut x_max = f32::MIN;
        for s in spans {
            let cx = s.bbox.x + s.bbox.width * 0.5;
            if (cx - median).abs() > max_extent {
                continue; // degenerate-CTM guard, same as the caller
            }
            let y_key = (s.bbox.y + s.bbox.height).round() as i32;
            let entry = lines.entry(y_key).or_default();
            entry.0.push((s.bbox.x, s.bbox.x + s.bbox.width));
            entry.1 += s.text.chars().filter(|c| !c.is_whitespace()).count();
            x_min = x_min.min(s.bbox.x);
            x_max = x_max.max(s.bbox.x + s.bbox.width);
        }
        let region_width = x_max - x_min;
        if lines.len() < 12 || region_width < 200.0 {
            return false;
        }

        let total_chars: usize = lines.values().map(|(_, c)| *c).sum();
        let mean_chars = total_chars as f32 / lines.len() as f32;

        // Largest within-line gap per line (≥ 6 pt suppresses word spacing);
        // record the gap midpoint X. Also flag full-width lines with no internal
        // gutter (display equations, full-width headings) so they neither support
        // nor veto the corridor — they are excluded from the coverage denominator
        // (Part 1b: display-math robustness, #536/arxiv_math).
        const MIN_GAP_PT: f32 = 6.0;
        let mut gap_positions: Vec<f32> = Vec::new();
        let mut full_width_lines = 0usize;
        let mut multi_gap_lines = 0usize;
        for (line_spans, _) in lines.values() {
            if line_spans.is_empty() {
                continue;
            }
            let mut sorted = line_spans.clone();
            sorted.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
            let line_left = sorted.first().map(|s| s.0).unwrap_or(0.0);
            let line_right = sorted.last().map(|s| s.1).unwrap_or(0.0);
            let mut largest_gap = 0.0_f32;
            let mut largest_mid = 0.0_f32;
            let mut significant_gaps = 0usize;
            for w in sorted.windows(2) {
                let gap = w[1].0 - w[0].1;
                if gap >= MIN_GAP_PT {
                    significant_gaps += 1;
                }
                if gap > largest_gap {
                    largest_gap = gap;
                    largest_mid = (w[0].1 + w[1].0) * 0.5;
                }
            }
            if (line_right - line_left) >= region_width * 0.9 && largest_gap < MIN_GAP_PT {
                full_width_lines += 1;
            }
            // A line with two or more wide internal gaps is a grid row (≥ 3
            // cells), not a two-column body line (one gutter). Used by the
            // short-line table discriminator below.
            if significant_gaps >= 2 {
                multi_gap_lines += 1;
            }
            if largest_gap >= MIN_GAP_PT {
                gap_positions.push(largest_mid);
            }
        }
        if gap_positions.len() < 12 {
            return false;
        }
        // Coverage denominator excludes full-width display rows.
        let eff_lines = lines.len().saturating_sub(full_width_lines).max(1);

        // Cluster gap midpoints (10 pt radius); find the dominant corridor.
        const CLUSTER_RADIUS_PT: f32 = 10.0;
        gap_positions.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        let mut best_size = 0usize;
        let mut best_center = 0.0_f32;
        let mut left = 0usize;
        let mut right = 0usize;
        let mut prefix: Vec<f32> = Vec::with_capacity(gap_positions.len() + 1);
        prefix.push(0.0);
        for &x in &gap_positions {
            prefix.push(prefix.last().unwrap() + x);
        }
        for &pivot in &gap_positions {
            while left < gap_positions.len() && gap_positions[left] < pivot - CLUSTER_RADIUS_PT {
                left += 1;
            }
            while right < gap_positions.len() && gap_positions[right] <= pivot + CLUSTER_RADIUS_PT {
                right += 1;
            }
            let count = right - left;
            if count > best_size {
                best_size = count;
                best_center = (prefix[right] - prefix[left]) / count as f32;
            }
        }

        // Gutter must sit near the page centre (0.30–0.70). A true two-column
        // body splits down the middle; a table's dominant gap (label column vs
        // data, or one of several cell boundaries) sits off-centre.
        let gutter_offset = best_center - x_min;
        let centre_ok =
            gutter_offset >= region_width * 0.30 && gutter_offset <= region_width * 0.70;
        if best_size < 16 || !centre_ok {
            return false;
        }

        if mean_chars > 20.0 {
            // Long-line two-column prose (the v0.3.57 accept path, unchanged
            // except the coverage denominator now excludes display rows):
            // concentration ≥ 62 %, coverage ≥ 50 % of (effective) lines.
            return best_size * 50 >= gap_positions.len() * 31 && best_size * 2 >= eff_lines;
        }

        // Short-line bodies (verse / lexicon / dictionary editions, #536): the
        // raw `mean_chars` floor used to reject these along with short-cell
        // tables. Admit them only under STRICTER, length-independent guards a
        // short-cell table cannot satisfy (Part 1a).
        let strict_concentration = best_size * 10 >= gap_positions.len() * 7; // ≥ 70 %
        let strict_coverage = best_size * 5 >= eff_lines * 3; // ≥ 60 % of lines
        if !(strict_concentration && strict_coverage) {
            return false;
        }
        // Column char-mass balance: each side of the gutter must carry ≥ 35 % of
        // the non-whitespace characters. A narrow label / verse-number column
        // paired with wide data is lopsided and rejected.
        let (mut left_chars, mut right_chars) = (0usize, 0usize);
        for s in spans {
            let cx = s.bbox.x + s.bbox.width * 0.5;
            if (cx - median).abs() > max_extent {
                continue;
            }
            let n = s.text.chars().filter(|c| !c.is_whitespace()).count();
            if cx < best_center {
                left_chars += n;
            } else {
                right_chars += n;
            }
        }
        let total = (left_chars + right_chars).max(1) as f32;
        if (left_chars as f32) < total * 0.35 || (right_chars as f32) < total * 0.35 {
            return false;
        }
        // Grid-row discriminator: a two-column body has ONE wide gap per line
        // (the gutter); a multi-cell numeric table has ≥ 2 wide gaps on most
        // rows (cell boundaries). Reject when the majority of lines are grid
        // rows — this is what keeps short-cell tables off the XY-cut path
        // without the raw `mean_chars` floor that also blocked short verse.
        multi_gap_lines * 2 <= eff_lines
    }

    /// RW-1: reading order for a **narrow-sidebar + wide-body** page (the MDPI /
    /// academic first-page layout: a full-width title band on top, a narrow left
    /// metadata column — Citation / Editor / Received / copyright — beside a wide
    /// body column). `is_multi_column_page` misreads this as two balanced columns
    /// and the XY-cut then slices the full-width title along the body gutter
    /// (§14.8.3: a block-level full-width element must NOT be column-assigned).
    ///
    /// Returns `Some(reordered_spans)` only when the layout is *confidently* this
    /// shape — emit order **band (title) + body, merged top→bottom, then the
    /// sidebar last** (the gold puts the title whole on top, then the body; the
    /// metadata sidebar is publisher furniture read last). Returns `None`
    /// otherwise so the normal XY-cut / row-aware path is unchanged. The gate is
    /// deliberately tight (gutter left-of-centre + a narrow left column + a wide
    /// right column + a full-width band near the top) so balanced two-column and
    /// single-column pages never reach it.
    pub(super) fn sidebar_body_reading_order(
        spans: &[crate::layout::TextSpan],
    ) -> Option<Vec<crate::layout::TextSpan>> {
        use crate::utils::safe_float_cmp;
        if spans.len() < 30 {
            return None;
        }
        let x_min = spans.iter().map(|s| s.bbox.left()).fold(f32::MAX, f32::min);
        let x_max = spans
            .iter()
            .map(|s| s.bbox.right())
            .fold(f32::MIN, f32::max);
        let y_min = spans.iter().map(|s| s.bbox.top()).fold(f32::MAX, f32::min);
        let y_max = spans
            .iter()
            .map(|s| s.bbox.bottom())
            .fold(f32::MIN, f32::max);
        let width = (x_max - x_min).max(1.0);
        let height = (y_max - y_min).max(1.0);
        if !(width.is_finite() && height.is_finite()) {
            return None;
        }

        // Cluster spans into baseline lines (top→bottom).
        let mut order: Vec<usize> = (0..spans.len()).collect();
        order.sort_by(|&a, &b| {
            safe_float_cmp(spans[b].bbox.bottom(), spans[a].bbox.bottom())
                .then_with(|| safe_float_cmp(spans[a].bbox.left(), spans[b].bbox.left()))
        });
        struct Line {
            top_y: f32,
            min_left: f32,
            max_right: f32,
            members: Vec<usize>,
        }
        let mut lines: Vec<Line> = Vec::new();
        for &i in &order {
            let s = &spans[i];
            let h = (s.bbox.bottom() - s.bbox.top()).abs().max(1.0);
            match lines.last_mut() {
                Some(l) if (l.top_y - s.bbox.bottom()).abs() <= h * 0.6 => {
                    l.min_left = l.min_left.min(s.bbox.left());
                    l.max_right = l.max_right.max(s.bbox.right());
                    l.members.push(i);
                }
                _ => lines.push(Line {
                    top_y: s.bbox.bottom(),
                    min_left: s.bbox.left(),
                    max_right: s.bbox.right(),
                    members: vec![i],
                }),
            }
        }
        if lines.len() < 12 {
            return None;
        }

        // Find the body-column left edge: the most common line-start that sits
        // well right of the page left margin (excludes the sidebar/title cluster
        // anchored at x_min). The gutter is just left of it.
        let body_left = {
            const BIN: f32 = 5.0;
            let nbins = ((width / BIN).ceil() as usize).clamp(1, 4096);
            let mut hist = vec![0usize; nbins];
            for l in &lines {
                if l.min_left > x_min + width * 0.10 {
                    let b = (((l.min_left - x_min) / BIN) as usize).min(nbins - 1);
                    hist[b] += 1;
                }
            }
            let (peak, &cnt) = hist.iter().enumerate().max_by_key(|(_, &c)| c)?;
            if cnt < 5 {
                return None; // no consistent body-column start
            }
            x_min + peak as f32 * BIN
        };
        // Gutter must be left-of-centre (a narrow sidebar, not a centred 2-col).
        let gutter = body_left - width * 0.02;
        if !(gutter > x_min + width * 0.12 && gutter < x_min + width * 0.45) {
            return None;
        }

        // A full-width TITLE/heading row is typeset as many narrow word spans, so
        // no single span satisfies the per-span band test below; its leftmost words
        // (right edge ≤ gutter) would be miswept into the SIDEBAR and emitted last,
        // shattering the title (e.g. an MDPI first page where the title sits in a
        // large font across the full width, above the metadata sidebar + body).
        // Detect these per LINE: a baseline line whose member words FLOW
        // CONTINUOUSLY across the gutter (collective extent crosses it, ≥40% of the
        // page width, and no large internal gap straddling the gutter) is a true
        // full-width band — its words are evenly spaced, unlike a sidebar-label +
        // body-line that merely SHARE a baseline (e.g. "Accepted: 1 March 2021"
        // next to "1. Introduction"), which leaves a wide empty gutter corridor
        // between the two columns. Members of such band lines are forced into the
        // BAND group so the whole title row stays together at its vertical slot.
        // The straddling-gap gate keeps shared sidebar/body baselines split.
        let mut band_line_members: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        const MAX_STRADDLE_GAP_FRAC: f32 = 0.06; // ≈ gutter-corridor width
                                                 // The title/author band sits ABOVE the body column (the body starts at the
                                                 // affiliations/abstract). Restrict band promotion to lines above the
                                                 // topmost PURE-BODY line (a line whose words all begin at/right of the
                                                 // gutter, with no left-of-gutter member). Below that Y the page is the
                                                 // two-column sidebar+body region, where a wide crossing line is a
                                                 // sidebar-label + body-line sharing a baseline (e.g. "Switzerland." next to
                                                 // "cancer, atrial fibrillation…") whose tight gutter corridor would
                                                 // otherwise pass the straddle-gap gate and wrongly glue the sidebar inline.
                                                 // Larger bottom-y == higher on the page (PDF user space).
        let body_top_y = lines
            .iter()
            .filter(|l| l.min_left >= gutter)
            .map(|l| l.top_y)
            .fold(f32::NEG_INFINITY, f32::max);
        for line in &lines {
            if !(line.min_left < gutter
                && line.max_right > gutter
                && (line.max_right - line.min_left) > width * 0.40)
            {
                continue;
            }
            // Only the top-of-page title/author band, above the body column.
            if body_top_y.is_finite() && line.top_y < body_top_y {
                continue;
            }
            // Largest gap between consecutive members that straddles the gutter.
            let mut xs: Vec<(f32, f32)> = line
                .members
                .iter()
                .map(|&i| (spans[i].bbox.left(), spans[i].bbox.right()))
                .collect();
            xs.sort_by(|a, b| safe_float_cmp(a.0, b.0));
            let mut max_straddle_gap = 0.0f32;
            let mut prev_right = f32::NEG_INFINITY;
            for &(l, r) in &xs {
                if prev_right.is_finite() && prev_right < gutter && l > gutter {
                    max_straddle_gap = max_straddle_gap.max(l - prev_right);
                }
                prev_right = prev_right.max(r);
            }
            if max_straddle_gap < width * MAX_STRADDLE_GAP_FRAC {
                for &i in &line.members {
                    band_line_members.insert(i);
                }
            }
        }

        // Classify each SPAN by the gutter. A publisher-metadata sidebar and the
        // body usually SHARE baselines (the metadata column interleaves with body
        // lines by Y), so a per-line cluster would fuse them into one full-width
        // line and hide the sidebar — classify per span instead. BAND = a span
        // genuinely spanning the gutter (a wide full-width title/heading), or a
        // member of a continuous full-width band LINE (above). SIDEBAR = a span
        // entirely left of the gutter. BODY = everything at/right of it.
        let mut band: Vec<usize> = Vec::new();
        let mut sidebar: Vec<usize> = Vec::new();
        let mut body: Vec<usize> = Vec::new();
        for (i, s) in spans.iter().enumerate() {
            let l = s.bbox.left();
            let r = s.bbox.right();
            if band_line_members.contains(&i)
                || (l < gutter && r > gutter && (r - l) > width * 0.40)
            {
                band.push(i);
            } else if r <= gutter {
                sidebar.push(i);
            } else {
                body.push(i);
            }
        }
        // A real sidebar/body are each multi-line.
        let line_count = |v: &[usize]| -> usize {
            let mut ys: Vec<f32> = v.iter().map(|&i| spans[i].bbox.bottom()).collect();
            ys.sort_by(|a, b| safe_float_cmp(*a, *b));
            ys.dedup_by(|a, b| (*a - *b).abs() <= 2.0);
            ys.len()
        };
        if line_count(&sidebar) < 5 || line_count(&body) < 8 {
            return None;
        }
        // Sidebar genuinely narrower than the body column.
        let col_width = |v: &[usize]| -> f32 {
            let lo = v
                .iter()
                .map(|&i| spans[i].bbox.left())
                .fold(f32::MAX, f32::min);
            let hi = v
                .iter()
                .map(|&i| spans[i].bbox.right())
                .fold(f32::MIN, f32::max);
            (hi - lo).max(0.0)
        };
        let sw = col_width(&sidebar);
        let bw = col_width(&body);
        if sw >= width * 0.45 || sw >= bw * 0.70 {
            return None; // left column not a narrow sidebar relative to the body
        }
        // ANTI-FORM discriminator. A bare narrow left column is geometrically
        // indistinguishable from a label:value form (Name:/Address:/Date:) or a
        // verse/margin-note page, and these PDFs carry NO background tint to anchor
        // the sidebar. The reliable signal is semantic: a publisher-metadata
        // sidebar carries recognisable furniture labels that never head a form
        // field or a body column. Require >=2 DISTINCT labels so ordinary narrow
        // columns and forms never engage this reordering.
        let side_text: String = {
            let mut t = String::new();
            for &i in &sidebar {
                t.push_str(&spans[i].text.to_lowercase());
                t.push(' ');
            }
            t
        };
        const FURNITURE: [&str; 12] = [
            "citation",
            "received",
            "accepted",
            "published",
            "copyright",
            "licensee",
            "academic editor",
            "publisher",
            "doi.org",
            "issn",
            "creative commons",
            "open access",
        ];
        let furniture_hits = FURNITURE.iter().filter(|k| side_text.contains(**k)).count();
        if furniture_hits < 2 {
            return None;
        }

        // Emit: band + body merged top→bottom (title stays on top, body flows,
        // any mid-body full-width element keeps its vertical slot), then the
        // sidebar furniture last. Spans within a line read left→right.
        let mut main: Vec<usize> = band;
        main.extend(body);
        let key = |idx: &usize| {
            let s = &spans[*idx];
            (s.bbox.bottom(), s.bbox.left())
        };
        main.sort_by(|a, b| {
            let (ay, ax) = key(a);
            let (by, bx) = key(b);
            safe_float_cmp(by, ay).then_with(|| safe_float_cmp(ax, bx))
        });
        sidebar.sort_by(|a, b| {
            let (ay, ax) = key(a);
            let (by, bx) = key(b);
            safe_float_cmp(by, ay).then_with(|| safe_float_cmp(ax, bx))
        });
        let mut out: Vec<crate::layout::TextSpan> = Vec::with_capacity(spans.len());
        for i in main.into_iter().chain(sidebar) {
            out.push(spans[i].clone());
        }
        Some(out)
    }
}
