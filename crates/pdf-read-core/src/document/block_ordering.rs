use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Order spans by a topological sort over text BLOCKS (a precede relation),
    /// for pages with genuine side-by-side regions (a two-column body, a
    /// two-column footer, a sidebar beside the body) that a flat row-aware (y,x)
    /// sort interleaves row-by-row (splicing one region's line into the other's).
    /// Returns `None` for any page WITHOUT two horizontally-disjoint,
    /// vertically-overlapping blocks (single-column and simple stacked layouts),
    /// so their output stays byte-identical.
    ///
    /// Coordinate convention (see `row_aware_span_cmp`): larger Y = higher on the
    /// page, read first; `bottom()` is a span's UPPER edge, `top()` its LOWER edge.
    pub(super) fn topological_block_order(
        spans: &[crate::layout::TextSpan],
    ) -> Option<Vec<crate::layout::TextSpan>> {
        use crate::utils::safe_float_cmp;
        if spans.len() < 8 {
            return None;
        }
        let hi = |s: &crate::layout::TextSpan| s.bbox.bottom(); // upper edge (larger y)
        let lo = |s: &crate::layout::TextSpan| s.bbox.top(); // lower edge (smaller y)
        let med_h = {
            let mut hs: Vec<f32> = spans
                .iter()
                .map(|s| (hi(s) - lo(s)).abs())
                .filter(|h| h.is_finite() && *h > 0.0)
                .collect();
            if hs.is_empty() {
                return None;
            }
            hs.sort_by(|a, b| safe_float_cmp(*a, *b));
            hs[hs.len() / 2].max(1.0)
        };

        // Item 4 (M3): measure the page's single central column gutter (if any).
        // Used below to forbid a same-line union ACROSS the gutter regardless of
        // the measured gap: on dense two-column pages with tight leading, an
        // over-wide advance can make a cross-gutter gap < med_h, fusing the two
        // columns into one block so the side_by_side gate then declines and the
        // page falls to a row-major interleave. `None` (single-column /
        // multi-corridor / off-centre) ⇒ the predicate is byte-identical.
        let gutter_x = Self::measure_single_central_gutter(spans)
            .or_else(|| Self::density_central_gutter(spans));

        // --- Union-find: connect spans in the same text region. Two spans join
        // iff they are on the same line and horizontally adjacent (a normal word
        // gap, NOT a column gutter), OR vertically stacked with overlapping X and
        // a small inter-line gap. A column gutter (≥ ~1 em of whitespace) never
        // connects, so left/right columns become separate blocks even when their
        // lines share Y bands. ---
        let n = spans.len();
        let mut parent: Vec<usize> = (0..n).collect();
        fn find(parent: &mut [usize], mut x: usize) -> usize {
            while parent[x] != x {
                parent[x] = parent[parent[x]];
                x = parent[x];
            }
            x
        }
        // Index spans by reading order so each only tests a local window.
        let mut ord: Vec<usize> = (0..n).collect();
        ord.sort_by(|&a, &b| {
            safe_float_cmp(hi(&spans[b]), hi(&spans[a]))
                .then_with(|| safe_float_cmp(spans[a].bbox.left(), spans[b].bbox.left()))
        });
        let x_overlap = |a: &crate::layout::TextSpan, b: &crate::layout::TextSpan| -> f32 {
            (a.bbox.right().min(b.bbox.right()) - a.bbox.left().max(b.bbox.left())).max(0.0)
        };
        for (p, &i) in ord.iter().enumerate() {
            let si = &spans[i];
            // Look ahead over a bounded window of following spans in reading order.
            for &j in ord.iter().skip(p + 1).take(40) {
                let sj = &spans[j];
                let dy_centers = ((hi(si) + lo(si)) - (hi(sj) + lo(sj))).abs() * 0.5;
                let same_line = dy_centers < med_h * 0.5;
                let connect = if same_line {
                    // Horizontal neighbour: gap below ~1 em (word space), not a gutter.
                    let gap = (si.bbox.left().max(sj.bbox.left()))
                        - (si.bbox.right().min(sj.bbox.right()));
                    // Item 4 (M3): never join two spans that straddle the measured
                    // central gutter (one wholly left of it, the other wholly
                    // right), independent of `gap` — a tight-leading over-wide
                    // advance can otherwise make the cross-gutter gap < med_h and
                    // fuse the columns. Purely subtractive: it can only PREVENT a
                    // union, never create one, so `gutter_x == None` is byte-identical.
                    let crosses_gutter = gutter_x.is_some_and(|gx| {
                        (si.bbox.right() <= gx && sj.bbox.left() >= gx)
                            || (sj.bbox.right() <= gx && si.bbox.left() >= gx)
                    });
                    !crosses_gutter && gap < med_h * 1.0
                } else {
                    // Vertical neighbour: overlap in X and a small inter-line gap.
                    let vgap = (lo(si).min(lo(sj)) - hi(si).max(hi(sj))).abs();
                    // (distance between the nearer edges)
                    let near = (lo(si) - hi(sj)).abs().min((lo(sj) - hi(si)).abs());
                    x_overlap(si, sj) > med_h * 0.3 && near < med_h * 1.5 && vgap < med_h * 6.0
                };
                if connect {
                    let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                    if ri != rj {
                        parent[ri] = rj;
                    }
                }
            }
        }

        // --- Build blocks from the union-find components. A BTreeMap (not a
        // HashMap) keys the components by root index so `into_values()` is
        // DETERMINISTIC — HashMap iteration order is randomized per run, which
        // would make the block order (and thus the extracted text) flaky for
        // pages where two blocks tie on the seed sort key. ---
        use std::collections::BTreeMap;
        let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for i in 0..n {
            let r = find(&mut parent, i);
            groups.entry(r).or_default().push(i);
        }
        struct Block {
            x0: f32,
            x1: f32,
            y_hi: f32,
            y_lo: f32,
            members: Vec<usize>,
        }
        let blocks: Vec<Block> = groups
            .into_values()
            .map(|members| {
                let mut b = Block {
                    x0: f32::MAX,
                    x1: f32::MIN,
                    y_hi: f32::MIN,
                    y_lo: f32::MAX,
                    members,
                };
                for &i in &b.members {
                    let s = &spans[i];
                    b.x0 = b.x0.min(s.bbox.left());
                    b.x1 = b.x1.max(s.bbox.right());
                    b.y_hi = b.y_hi.max(hi(s));
                    b.y_lo = b.y_lo.min(lo(s));
                }
                b
            })
            .collect();
        if blocks.len() < 2 {
            return None;
        }

        // A STRUCTURED/TABULAR page (a chess-move table, a data grid, a TOC with a
        // page-number rail) shatters into many tiny fragment blocks the union-find
        // cannot coalesce — isolated cells, page numbers, single-letter column
        // heads — interleaved with the column runs. Flowing multi-column prose and
        // a sidebar+body do not: they are a handful of big blocks. So when fragment
        // blocks (< 4 spans) outnumber the substantial ones, the page is tabular
        // and must stay row-aware, NOT be read column-major.
        let fragments = blocks.iter().filter(|b| b.members.len() < 4).count();
        if fragments > blocks.len() - fragments {
            return None;
        }

        // Item 4 follow-up (M3): if the page has a clean central column gutter but
        // the union-find STILL produced a block that fuses the two columns across
        // it, the topological emit would interleave them (the block's spans get
        // sorted row-aware within the block). This happens on dense two-column
        // bodies where a producer-malformed full-width fragment, or a chain of
        // vertical unions through a wide line, bridges the columns despite the
        // same-line cross-gutter veto. A correct two-column block decomposition
        // has NO block straddling the gutter with substantial content on BOTH
        // sides. When one exists, bail to None so the dispatch falls through to
        // the band-aware column-major reader (`classifier_column_gutter` /
        // `reorder_column_major_with_bands`), which separates the columns and
        // re-emits full-width bands at their own Y. Gated on a measured gutter, so
        // pages without one (the common case) are byte-identical.
        if let Some(gx) = gutter_x {
            let fused = blocks.iter().any(|b| {
                if b.x0 >= gx - med_h || b.x1 <= gx + med_h {
                    return false; // does not straddle the gutter
                }
                // Count this block's members clearly on each side of the gutter.
                let mut l = 0usize;
                let mut r = 0usize;
                for &i in &b.members {
                    let s = &spans[i];
                    if s.bbox.right() <= gx {
                        l += 1;
                    } else if s.bbox.left() >= gx {
                        r += 1;
                    }
                }
                // Substantial content on BOTH sides ⇒ fused columns, not a band.
                l >= 4 && r >= 4
            });
            if fused {
                return None;
            }
        }

        // --- GATE: require ≥2 blocks that are horizontally DISJOINT yet overlap
        // in Y (genuine side-by-side regions). Single-column / stacked layouts
        // have none, so they return None and stay byte-identical. ---
        let y_ov = |a: &Block, b: &Block| (a.y_hi.min(b.y_hi) - a.y_lo.max(b.y_lo)) > med_h * 0.5;
        let x_disjoint = |a: &Block, b: &Block| a.x1 <= b.x0 || b.x1 <= a.x0;
        // Character density per block (≈ chars per text line). A page-number
        // column in a TOC, or the value column of a label:value form/table, is
        // text-SPARSE (a few chars per line); genuine prose columns and a
        // publisher metadata sidebar are text-DENSE. Used to reject row-paired
        // tables/TOCs/forms (which must read row-wise, NOT column-major).
        let char_density = |b: &Block| -> f32 {
            let mut ys: Vec<f32> = b.members.iter().map(|&i| hi(&spans[i])).collect();
            ys.sort_by(|p, q| safe_float_cmp(*p, *q));
            let mut lines = 1usize;
            for w in ys.windows(2) {
                if (w[1] - w[0]).abs() > med_h * 0.6 {
                    lines += 1;
                }
            }
            let chars: usize = b
                .members
                .iter()
                .map(|&i| spans[i].text.trim().chars().count())
                .sum();
            chars as f32 / lines as f32
        };
        // Number of baseline rows a block spans (same Y-clustering as char_density).
        let block_lines = |b: &Block| -> usize {
            let mut ys: Vec<f32> = b.members.iter().map(|&i| hi(&spans[i])).collect();
            ys.sort_by(|p, q| safe_float_cmp(*p, *q));
            let mut lines = 1usize;
            for w in ys.windows(2) {
                if (w[1] - w[0]).abs() > med_h * 0.6 {
                    lines += 1;
                }
            }
            lines
        };

        // Both side-by-side blocks must be SUBSTANTIAL, text-DENSE, multi-line
        // regions that overlap over several lines — a genuine 2-column body/footer
        // or a sidebar+body. Incidental overlaps (a drop cap, a page number, a
        // margin note, a fragmented poem line) involve tiny blocks or a sliver of
        // Y-overlap; row-paired tables/TOCs/forms have a text-sparse value column.
        // Neither must engage the reorder, or single-column poetry, decorated
        // pages, and TOCs scramble.
        let side_by_side = blocks.iter().enumerate().any(|(i, a)| {
            blocks.iter().skip(i + 1).any(|b| {
                x_disjoint(a, b)
                    && a.members.len() >= 8
                    && b.members.len() >= 8
                    && (a.y_hi.min(b.y_hi) - a.y_lo.max(b.y_lo)) > med_h * 3.0
                    // Each side must be a genuine MULTI-LINE column (≥ 4 rows). A
                    // single-column page whose body happens to end in just a
                    // couple of lines can have a wide intra-line word gap (a
                    // sentence space after a period) split those lines into two
                    // x-disjoint blocks that otherwise pass this gate and emit as
                    // fake columns (alice_old "Looking-Glass House" p.226). A real
                    // two-column body / sidebar spans many rows.
                    && block_lines(a) >= 4
                    && block_lines(b) >= 4
                    && char_density(a) >= 12.0
                    && char_density(b) >= 12.0
                    // The two side-by-side blocks must be the page's DOMINANT
                    // content (≥ half the spans). A genuine 2-column body or
                    // sidebar+body lives in two big blocks; a table / chess
                    // diagram / dense diagram fragments into many small blocks
                    // that the union-find cannot coalesce, so the dominant pair
                    // never reaches half — leaving such pages on the row-aware path.
                    && (a.members.len() + b.members.len()) * 2 >= n
            })
        });
        if !side_by_side {
            return None;
        }

        // --- Topological order (two precede rules). A precedes B if they
        // overlap in X and A is above B (vertical stack), OR A is left of B and
        // they overlap in Y (side-by-side columns: left first). DFS with a visited
        // guard appends a block only after all its predecessors, and terminates on
        // any rule cycle. ---
        let nb = blocks.len();
        let before = |a: &Block, b: &Block| -> bool {
            let x_ov = (a.x1.min(b.x1) - a.x0.max(b.x0)) > med_h * 0.3;
            if x_ov && a.y_hi > b.y_hi && a.y_lo > b.y_lo {
                return true; // A stacked above B
            }
            if a.x1 <= b.x0 && y_ov(a, b) {
                return true; // A is the left column of a side-by-side pair
            }
            false
        };
        // Kahn's algorithm over the `before` relation. The previous
        // iterative DFS re-pushed every unvisited predecessor each time a
        // node was expanded (no on-stack marking), which is exponential in
        // stack growth on block graphs with heavy fan-in — a dense
        // equation page produced tens of gigabytes of stack and an OOM
        // kill. Kahn's is O(V^2) for the edge scan and O(V+E) after,
        // visits each block exactly once, and terminates unconditionally;
        // ready blocks are drained in reading order (top-left first) for
        // a stable result, matching the old seed order.
        let mut result_blocks: Vec<usize> = Vec::with_capacity(nb);
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); nb];
        let mut indegree: Vec<usize> = vec![0; nb];
        for a in 0..nb {
            for b in 0..nb {
                if a != b && before(&blocks[a], &blocks[b]) {
                    // a must come before b.
                    preds[a].push(b);
                    indegree[b] += 1;
                }
            }
        }
        let seed_order = |a: usize, b: usize| {
            safe_float_cmp(blocks[b].y_hi, blocks[a].y_hi)
                .then_with(|| safe_float_cmp(blocks[a].x0, blocks[b].x0))
        };
        // Kept sorted in REVERSE reading order so pop() takes the
        // top-left-most ready block.
        let mut ready: Vec<usize> = (0..nb).filter(|&i| indegree[i] == 0).collect();
        ready.sort_by(|&a, &b| seed_order(b, a));
        let mut emitted = vec![false; nb];
        while let Some(bi) = ready.pop() {
            // `ready` is kept sorted with the NEXT block last (reverse
            // reading order), so pop() takes the top-left-most.
            if emitted[bi] {
                continue;
            }
            emitted[bi] = true;
            result_blocks.push(bi);
            let mut newly_ready = false;
            for &succ in &preds[bi] {
                indegree[succ] -= 1;
                if indegree[succ] == 0 {
                    ready.push(succ);
                    newly_ready = true;
                }
            }
            if newly_ready {
                ready.sort_by(|&a, &b| seed_order(b, a));
            }
        }
        // The `before` relation is acyclic by construction (edges strictly
        // decrease y within a band or strictly increase x across columns),
        // but guard against float pathologies leaving blocks unemitted:
        // append any remainder in reading order rather than dropping text.
        if result_blocks.len() < nb {
            let mut rest: Vec<usize> = (0..nb).filter(|&i| !emitted[i]).collect();
            rest.sort_by(|&a, &b| seed_order(a, b));
            result_blocks.extend(rest);
        }

        // --- Emit: each block's spans in reading order (y desc, x asc). ---
        let mut out: Vec<crate::layout::TextSpan> = Vec::with_capacity(n);
        for &bi in &result_blocks {
            let mut members = blocks[bi].members.clone();
            members.sort_by(|&a, &b| {
                safe_float_cmp(hi(&spans[b]), hi(&spans[a]))
                    .then_with(|| safe_float_cmp(spans[a].bbox.left(), spans[b].bbox.left()))
            });
            for i in members {
                out.push(spans[i].clone());
            }
        }
        if out.len() == n {
            Some(out)
        } else {
            None
        }
    }

    /// True if the spans cluster into lines whose leftmost X positions
    /// form ≥ 2 distinct peaks separated by a clear gutter.
    ///
    /// Body-level word spans fill the X axis continuously, so the
    /// span-center histogram cannot tell two-column body text apart
    /// from a single-column page with varied line lengths. The line-
    /// start histogram does: in two-column body text most lines start
    /// at one of two X positions (left-column-start or right-column-
    /// start), and the wide gutter between the columns produces a
    /// long zero-count stretch.
    pub(super) fn has_bimodal_line_starts(spans: &[crate::layout::TextSpan]) -> bool {
        const Y_BAND: f32 = 2.0;
        const BIN_PT: f32 = 5.0;
        const MIN_PEAK_COUNT: usize = 4;
        const MIN_GUTTER_PT: f32 = 30.0;

        if spans.len() < 24 {
            return false;
        }

        // Cluster spans into lines by Y (descending so top-of-page first).
        let mut lines: Vec<(f32, f32)> = Vec::new(); // (y, line_x_min)
        let mut sorted = spans.to_vec();
        sorted.sort_by(|a, b| {
            crate::utils::safe_float_cmp(b.bbox.y, a.bbox.y)
                .then_with(|| crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x))
        });

        let mut current_y: Option<f32> = None;
        let mut current_xmin: f32 = f32::INFINITY;
        for s in &sorted {
            match current_y {
                Some(y) if (y - s.bbox.y).abs() <= Y_BAND => {
                    current_xmin = current_xmin.min(s.bbox.x);
                }
                _ => {
                    if let Some(y) = current_y {
                        if current_xmin.is_finite() {
                            lines.push((y, current_xmin));
                        }
                    }
                    current_y = Some(s.bbox.y);
                    current_xmin = s.bbox.x;
                }
            }
        }
        if let Some(y) = current_y {
            if current_xmin.is_finite() {
                lines.push((y, current_xmin));
            }
        }
        if lines.len() < 16 {
            return false;
        }

        // Bin line-start X positions.
        let xmin = lines.iter().map(|(_, x)| *x).fold(f32::INFINITY, f32::min);
        let xmax = lines
            .iter()
            .map(|(_, x)| *x)
            .fold(f32::NEG_INFINITY, f32::max);
        if !(xmin.is_finite() && xmax.is_finite()) || xmax - xmin < MIN_GUTTER_PT {
            return false;
        }
        let bin_count = (((xmax - xmin) / BIN_PT).ceil() as usize).max(1);
        if bin_count > 4096 {
            return false; // degenerate CTM
        }
        let mut hist = vec![0usize; bin_count];
        for (_, x) in &lines {
            let idx = (((x - xmin) / BIN_PT) as usize).min(bin_count - 1);
            hist[idx] += 1;
        }

        // Scan for ≥ 2 peaks (count ≥ MIN_PEAK_COUNT) with a long
        // zero-count run between them.
        let mut peaks: Vec<usize> = Vec::new(); // bin indices (peak center)
        let mut in_peak = false;
        let mut peak_start = 0usize;
        for (i, &c) in hist.iter().enumerate() {
            if c >= MIN_PEAK_COUNT {
                if !in_peak {
                    peak_start = i;
                    in_peak = true;
                }
            } else if c == 0 && in_peak {
                peaks.push((peak_start + i.saturating_sub(1)) / 2);
                in_peak = false;
            }
        }
        if in_peak {
            peaks.push((peak_start + hist.len() - 1) / 2);
        }
        if peaks.len() < 2 {
            return false;
        }

        // Check gutter: at least one pair of consecutive peaks must
        // have ≥ MIN_GUTTER_PT zero-count between them.
        let gutter_bins = (MIN_GUTTER_PT / BIN_PT) as usize;
        for w in peaks.windows(2) {
            let a = w[0];
            let b = w[1];
            if b <= a {
                continue;
            }
            let zeros = hist[a + 1..b].iter().filter(|&&c| c == 0).count();
            if zeros >= gutter_bins {
                return true;
            }
        }
        false
    }

    /// Numeric value 0–9 of a folio (page-number) digit, or `None` if `c` is
    /// not one. Scoped to the decimal-digit blocks that actually appear as
    /// page folios: ASCII, Arabic-Indic, Extended Arabic-Indic (Persian/Urdu),
    /// Devanagari, and full-width. Deliberately narrower than
    /// `char::is_numeric()` (which also matches `½`, `①`, superscripts) and
    /// wider than `char::is_ascii_digit()`. CJK ideographic numerals
    /// (`一二三…`) are intentionally excluded — they are not Unicode `Nd`, and
    /// collapsing them would over-normalize real headings (`第一章` → `第#章`).
    ///
    /// `char::to_digit(10)` cannot stand in here: it is ASCII-only and returns
    /// `None` for `'٥'` / `'५'` / `'５'`, so each block is mapped to its zero
    /// code point directly.
    pub(super) fn folio_digit_value(c: char) -> Option<u32> {
        let cp = c as u32;
        let base = match cp {
            0x0030..=0x0039 => 0x0030, // ASCII 0-9
            0x0660..=0x0669 => 0x0660, // Arabic-Indic
            0x06F0..=0x06F9 => 0x06F0, // Extended Arabic-Indic (Persian/Urdu)
            0x0966..=0x096F => 0x0966, // Devanagari
            0xFF10..=0xFF19 => 0xFF10, // Full-width
            _ => return None,
        };
        Some(cp - base)
    }

    /// Unicode-aware decimal-digit predicate for page folios. See
    /// [`Self::folio_digit_value`] for the supported blocks and rationale.
    pub(super) fn is_folio_digit(c: char) -> bool {
        Self::folio_digit_value(c).is_some()
    }

    /// Normalize a span's text for cross-page signature matching.
    /// Collapses whitespace and replaces digit runs with `#` so that page
    /// numbers ("Page 1 of 10", "Page 2 of 10") collapse to one signature.
    /// Non-Latin folio digits (Arabic-Indic, Persian, Devanagari, full-width)
    /// collapse too, so folios paginated in those scripts share one signature.
    pub(super) fn normalize_artifact_signature(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut in_digit_run = false;
        let mut last_was_space = true;
        for c in text.chars() {
            if Self::is_folio_digit(c) {
                if !in_digit_run {
                    out.push('#');
                    in_digit_run = true;
                }
                last_was_space = false;
            } else if c.is_whitespace() {
                if !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
                in_digit_run = false;
            } else {
                out.push(c);
                last_was_space = false;
                in_digit_run = false;
            }
        }
        out.trim().to_string()
    }

    /// Item 6B (M5): does a running-band literal look like a CONSTANT-text
    /// pagination / citation string — a DOI, a journal volume/issue/article
    /// reference, or a journal URL host — accompanied by a digit? Such strings
    /// recur identically on every page (so the varying-literal gate never catches
    /// them) yet are furniture that leaks into the body. The gate is deliberately
    /// narrow: it requires a recognised citation/URL token AND a digit, so a
    /// repeated facility name, document title, or ordinary sentence is NEVER
    /// matched (miss-rather-than-drop — a false positive deletes real content).
    pub(super) fn looks_like_stable_pagination(literal: &str) -> bool {
        let l = literal.to_ascii_lowercase();
        // The digit gate is script-aware (a non-Latin folio digit still
        // qualifies); the citation/URL keyword tokens below remain English-
        // only by design — keyword universality is tracked separately.
        if !l.chars().any(Self::is_folio_digit) {
            return false;
        }
        if l.contains("doi.org") || l.contains("doi:") || l.contains("/doi/") {
            return true;
        }
        // Journal volume/issue/article reference. NB: "no." is deliberately
        // EXCLUDED — it also matches government-form control numbers like
        // "OMB No. 1545-0115", which are form content, not running furniture.
        if ["volume", "vol.", "article", "issue"]
            .iter()
            .any(|kw| l.contains(kw))
        {
            return true;
        }
        // Journal URL host in a running footer, e.g. "www.frontiersin.org 1".
        l.contains("www.") && (l.contains(".org") || l.contains(".com") || l.contains(".net"))
    }

    /// A page is treated as vertical-writing (CJK tategaki, 縦書き) when a
    /// majority of its non-empty text spans were rendered in WMode 1. The
    /// writing mode comes from the PDF's own `/WMode` (captured on each span
    /// via `GraphicsState::text_wmode`), so this is authoritative — a
    /// horizontal page (WMode 0) is never misclassified, and its
    /// running-header/footer detection is unchanged.
    pub(super) fn page_is_vertical(spans: &[crate::layout::TextSpan]) -> bool {
        let mut vertical = 0usize;
        let mut total = 0usize;
        for s in spans {
            if s.text.trim().is_empty() {
                continue;
            }
            total += 1;
            if s.wmode == 1 {
                vertical += 1;
            }
        }
        total > 0 && vertical * 2 > total
    }

    /// Is `bbox` inside the candidate running-header/footer band for a page of
    /// the given dimensions? Horizontal pages use the top/bottom 12% strips.
    /// Vertical-writing (tategaki) pages *additionally* use the left/right 12%
    /// strips — the outer edge where CJK vertical folios and running heads
    /// conventionally sit, rather than across the top/bottom edge. The side
    /// strips are additive (the top/bottom test still applies), so this only
    /// ever widens detection, never narrows it.
    pub(super) fn in_chrome_band(
        bbox: &crate::geometry::Rect,
        page_width: f32,
        page_height: f32,
        vertical: bool,
    ) -> bool {
        let vband = page_height * 0.12;
        if bbox.y < vband || bbox.y + bbox.height > page_height - vband {
            return true;
        }
        if vertical {
            let hband = page_width * 0.12;
            if bbox.x < hband || bbox.x + bbox.width > page_width - hband {
                return true;
            }
        }
        false
    }

    /// Ensure running-artifact signatures are computed (once) and return a
    /// clone for matching. The computation scans every page's raw spans,
    /// collects normalized text that appears in the top/bottom 12% band (and,
    /// on vertical-writing pages, the left/right 12% band), and keeps entries
    /// that recur on >=50% of pages.
    /// Article threads for this document, parsed once and shared.
    /// [`crate::structure::parse_article_threads`] walks the entire page tree,
    /// and reading-order resolution asks for them on every page.
    pub(crate) fn cached_article_threads(
        &self,
    ) -> std::sync::Arc<Vec<crate::structure::ArticleThread>> {
        if let Some(cached) = self.article_threads_cache.lock_or_recover().as_ref() {
            return std::sync::Arc::clone(cached);
        }
        let threads = std::sync::Arc::new(crate::structure::parse_article_threads(self));
        *self.article_threads_cache.lock_or_recover() = Some(std::sync::Arc::clone(&threads));
        threads
    }
}
