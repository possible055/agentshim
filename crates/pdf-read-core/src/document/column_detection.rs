use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Detect a marginalia column (Item 2 / M2): a narrow, sparse, body-aligned
    /// numeric rail at the extreme left or right of the page — manuscript line
    /// numbers (`118 119 120 …`), a folio rail. Returns the indices of the rail
    /// spans (into `spans`) so the caller can lift them OUT of the body before
    /// geometric column dispatch (a rail otherwise injects a spurious second
    /// corridor / sparse block that disqualifies prose/topo detection) and
    /// re-append them at the end of the reading order.
    ///
    /// Tight 7-gate conjunction so it is a strict no-op (`None`) on ordinary
    /// pages and never lifts a genuine narrow first column (which is text-DENSE
    /// and multi-word → fails the sparsity + numeric-shape gates). `None` keeps
    /// the caller byte-identical.
    pub(super) fn lift_marginalia_column(spans: &[crate::layout::TextSpan]) -> Option<Vec<usize>> {
        use crate::utils::safe_float_cmp;
        // Gate 1: a substantial multi-span body page.
        let texties: Vec<usize> = (0..spans.len())
            .filter(|&i| {
                !spans[i].text.trim().is_empty()
                    && spans[i].bbox.x.is_finite()
                    && spans[i].bbox.width.is_finite()
                    && spans[i].bbox.width > 0.0
            })
            .collect();
        if texties.len() < 12 {
            return None;
        }
        let median = |mut v: Vec<f32>| -> Option<f32> {
            if v.is_empty() {
                return None;
            }
            v.sort_by(|a, b| safe_float_cmp(*a, *b));
            Some(v[v.len() / 2].max(1.0))
        };
        let med_fs = median(
            texties
                .iter()
                .filter(|&&i| spans[i].text.trim().chars().count() >= 2 && spans[i].font_size > 0.0)
                .map(|&i| spans[i].font_size)
                .collect(),
        )?;
        let med_h = median(
            texties
                .iter()
                .map(|&i| spans[i].bbox.height.abs())
                .filter(|h| h.is_finite() && *h > 0.0)
                .collect(),
        )?;
        let cmin = texties
            .iter()
            .map(|&i| spans[i].bbox.x)
            .fold(f32::INFINITY, f32::min);
        let cmax = texties
            .iter()
            .map(|&i| spans[i].bbox.x + spans[i].bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        let content_w = cmax - cmin;
        if content_w < 100.0 {
            return None;
        }
        let xband = 3.0 * med_fs; // Gate 2: narrow strip width.

        // M2 targets LEFT-margin manuscript line-number rails (the documented
        // mechanism — "narrow left-margin numerals woven into the prose stream").
        // A right-margin narrow numeric column is predominantly a TOC/table-of-
        // contents PAGE-NUMBER reference that pairs 1:1 with its entry row;
        // lifting it would regroup the page numbers away from their entries (a
        // reorder that hurts TOC pages, observed on CFR Title 36). So only the
        // left rail is considered. The symmetric right-side geometry below is
        // retained so a future TOC-discriminating gate can re-enable it safely.
        for left_side in core::iter::once(true) {
            let in_band = |i: usize| -> bool {
                let l = spans[i].bbox.x;
                let r = spans[i].bbox.x + spans[i].bbox.width;
                if left_side {
                    r <= cmin + xband
                } else {
                    l >= cmax - xband
                }
            };
            let strip: Vec<usize> = texties.iter().copied().filter(|&i| in_band(i)).collect();
            if strip.len() < 3 {
                continue;
            }
            let strip_set: std::collections::HashSet<usize> = strip.iter().copied().collect();
            let body: Vec<usize> = texties
                .iter()
                .copied()
                .filter(|i| !strip_set.contains(i))
                .collect();
            if body.len() < 8 {
                continue; // need a real body to order around the rail
            }

            // Gate 3: SPARSE (few chars per line).
            let strip_refs: Vec<&crate::layout::TextSpan> =
                strip.iter().map(|&i| &spans[i]).collect();
            if Self::block_char_density(&strip_refs, med_h) >= 4.0 {
                continue;
            }

            // Gate 7: at least 3 rail lines (a recurring rail, not a stray number).
            let mut ys: Vec<f32> = strip.iter().map(|&i| spans[i].bbox.bottom()).collect();
            ys.sort_by(|p, q| safe_float_cmp(*p, *q));
            let lines = 1 + ys
                .windows(2)
                .filter(|w| (w[1] - w[0]).abs() > med_h * 0.6)
                .count();
            if lines < 3 {
                continue;
            }

            // Gate 6: NUMERIC-SHAPE — ≥70% pure digits or ≤3-char tokens. This is
            // the discriminator vs a real narrow prose column (multi-word lines).
            let numeric = strip
                .iter()
                .filter(|&&i| {
                    let t = spans[i].text.trim();
                    (!t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
                        || t.chars().count() <= 3
                })
                .count();
            if (numeric as f32) < 0.70 * strip.len() as f32 {
                continue;
            }

            // Gate 4: DETACHED — a clear ≥18 pt empty gutter between the rail's
            // inner edge and the body's outer edge (the rail is geometrically
            // separate, not just the first words of body lines).
            let (strip_inner, body_outer) = if left_side {
                (
                    strip
                        .iter()
                        .map(|&i| spans[i].bbox.x + spans[i].bbox.width)
                        .fold(f32::NEG_INFINITY, f32::max),
                    body.iter()
                        .map(|&i| spans[i].bbox.x)
                        .fold(f32::INFINITY, f32::min),
                )
            } else {
                (
                    strip
                        .iter()
                        .map(|&i| spans[i].bbox.x)
                        .fold(f32::INFINITY, f32::min),
                    body.iter()
                        .map(|&i| spans[i].bbox.x + spans[i].bbox.width)
                        .fold(f32::NEG_INFINITY, f32::max),
                )
            };
            let gutter = if left_side {
                body_outer - strip_inner
            } else {
                strip_inner - body_outer
            };
            if gutter < 18.0 {
                continue;
            }

            // Gate 5: BODY-ALIGNED — the rail runs ALONGSIDE the body (Y-overlap
            // > half the rail height), not above/below it.
            let (sy0, sy1) = strip
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &i| {
                    (
                        a.min(spans[i].bbox.y),
                        b.max(spans[i].bbox.y + spans[i].bbox.height),
                    )
                });
            let (by0, by1) = body
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &i| {
                    (
                        a.min(spans[i].bbox.y),
                        b.max(spans[i].bbox.y + spans[i].bbox.height),
                    )
                });
            let overlap = sy1.min(by1) - sy0.max(by0);
            if overlap <= 0.5 * (sy1 - sy0).max(1.0) {
                continue;
            }

            return Some(strip);
        }
        None
    }

    pub(super) fn prose_two_column_gutter(spans: &[crate::layout::TextSpan]) -> Option<f32> {
        let body: Vec<&crate::layout::TextSpan> = spans
            .iter()
            .filter(|s| {
                !s.text.trim().is_empty()
                    && s.bbox.width > 0.0
                    && s.bbox.x.is_finite()
                    && s.bbox.width.is_finite()
            })
            .collect();
        if body.len() < 8 {
            return None;
        }
        let cmin = body.iter().map(|s| s.bbox.x).fold(f32::INFINITY, f32::min);
        let cmax = body
            .iter()
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        let content_w = cmax - cmin;
        if content_w < 100.0 {
            return None;
        }
        // Exactly one clean corridor near mid-page (bridge-excluded). 0 = single
        // column; ≥2 = grid/form/table.
        let mut boxes: Vec<(f32, f32)> = body
            .iter()
            .filter(|s| s.bbox.width <= 0.6 * content_w)
            .map(|s| (s.bbox.x, s.bbox.x + s.bbox.width))
            .collect();
        if boxes.len() < 8 {
            return None;
        }
        boxes.sort_by(|a, b| crate::utils::safe_float_cmp(a.0, b.0));
        let mut cover = boxes[0].1;
        let (mut corridors, mut gutter_x) = (0usize, 0.0f32);
        for &(l, r) in &boxes[1..] {
            if l - cover >= 12.0 {
                corridors += 1;
                gutter_x = (cover + l) * 0.5;
            }
            cover = cover.max(r);
        }
        if corridors != 1 || !(0.30..=0.70).contains(&((gutter_x - cmin) / content_w)) {
            return None;
        }
        // Column count via left-edge clustering. The coverage sweep above counts
        // *one* corridor whenever a single wide span in a column reaches past the
        // next column's start, hiding the real inter-column gap — so a two-page
        // spread (4 columns) collapses to its spread midline and reads as a clean
        // 2-column page, whose halves then each merge two real columns into an
        // interleaved row-major mess. Cluster the (non-full-width) span left edges
        // and require EXACTLY two significant column starts; anything else
        // (single column, 3+ columns, N-up spread) is rejected.
        {
            let mut lefts: Vec<f32> = boxes.iter().map(|&(l, _)| l).collect();
            lefts.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
            let clust_gap = 0.08 * content_w;
            let mut counts: Vec<usize> = Vec::new();
            let mut run = 1usize;
            let mut prev = lefts[0];
            for &v in &lefts[1..] {
                if v - prev > clust_gap {
                    counts.push(run);
                    run = 0;
                }
                run += 1;
                prev = v;
            }
            counts.push(run);
            // A "significant" column start carries ≥15% of the column-eligible
            // spans (a hanging-indent continuation merges into its start cluster
            // because its offset is far below clust_gap).
            let min_sig = (0.15 * lefts.len() as f32).ceil() as usize;
            let sig = counts.iter().filter(|&&c| c >= min_sig).count();
            if sig != 2 {
                return None;
            }
        }
        // Per-column region classification. The genuine discriminator between a
        // two-column PROSE/REFERENCE body (read column-major) and a table / form /
        // TOC that merely has one central gap (read row-wise) is the STRUCTURE of
        // each column, not a cross-gutter row-balance ratio. A cross-gutter
        // row-alignment gate measures alignment that ragged reference lists and
        // dense results columns do not have, so those were wrongly rejected and
        // fell to a row-major interleave. Classifying each half on its own
        // structure admits them while still rejecting tables/forms (which classify
        // as Table/Form). See `examples/classify_probe.rs`.
        let body_side = |want_left: bool| -> Vec<usize> {
            spans
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    !s.text.trim().is_empty()
                        && s.bbox.width > 0.0
                        && s.bbox.x.is_finite()
                        && s.bbox.width.is_finite()
                        && ((s.bbox.x + s.bbox.width * 0.5 < gutter_x) == want_left)
                })
                .map(|(i, _)| i)
                .collect()
        };
        let left_class = crate::layout::classify_region(spans, &body_side(true));
        let right_class = crate::layout::classify_region(spans, &body_side(false));
        if left_class.is_reorderable_column() && right_class.is_reorderable_column() {
            return Some(gutter_x);
        }
        // Fallback: the v0.3.66 cross-gutter content-balance test. The per-column
        // classifier (above) admits ragged reference lists / dense results columns
        // the balance test rejected, but it also REJECTS some genuine balanced
        // two-column PROSE the balance test accepted — short, ragged verse/body
        // lines on a narrow-gutter page (a reference-Bible / two-column page with a
        // full-width title, issue #734). Without this they fall off the
        // column-major path and the first row interleaves across the gutter. Tried
        // only AFTER the classifier (academic pages keep the classifier path) and
        // behind the same corridor + two-column-start preamble, so single-column /
        // grid / N-up pages never reach it.
        if Self::two_column_rows_balanced(spans, gutter_x) {
            return Some(gutter_x);
        }
        None
    }

    /// v0.3.66 cross-gutter content-balance test: true when spanning rows carry
    /// substantial text on BOTH sides of `gutter_x` (prose), not a short
    /// right-hand value / page number (form / TOC). Fallback for
    /// `prose_two_column_gutter` after the per-column classifier declines.
    pub(super) fn two_column_rows_balanced(
        spans: &[crate::layout::TextSpan],
        gutter_x: f32,
    ) -> bool {
        let mut ordered: Vec<&crate::layout::TextSpan> = spans
            .iter()
            .filter(|s| {
                !s.text.trim().is_empty()
                    && s.bbox.width > 0.0
                    && s.bbox.x.is_finite()
                    && s.bbox.width.is_finite()
            })
            .collect();
        ordered.sort_by(|a, b| crate::utils::safe_float_cmp(b.bbox.y, a.bbox.y));
        let (mut total, mut spanning, mut short_r) = (0usize, 0usize, 0usize);
        let (mut lefts, mut rights): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i < ordered.len() {
            let y0 = ordered[i].bbox.y;
            let (mut lc, mut rc) = (0usize, 0usize);
            while i < ordered.len() && (ordered[i].bbox.y - y0).abs() <= 3.0 {
                let s = ordered[i];
                let n = s.text.trim().chars().count();
                if s.bbox.x + s.bbox.width * 0.5 < gutter_x {
                    lc += n;
                } else {
                    rc += n;
                }
                i += 1;
            }
            total += 1;
            if lc > 0 && rc > 0 {
                spanning += 1;
                lefts.push(lc);
                rights.push(rc);
                if rc < 15 {
                    short_r += 1;
                }
            }
        }
        if total < 6 || spanning == 0 || (spanning as f32) < 0.60 * total as f32 {
            return false;
        }
        if (short_r as f32) > 0.30 * spanning as f32 {
            return false;
        }
        let med = |v: &mut [usize]| -> f32 {
            v.sort_unstable();
            v[v.len() / 2] as f32
        };
        let (ml, mr) = (med(&mut lefts), med(&mut rights));
        mr >= 25.0 && (0.45..=2.2).contains(&(mr / ml.max(1.0)))
    }

    /// Robust classifier-gated two-column detector for bodies the clean corridor
    /// sweep (`prose_two_column_gutter`) and `is_multi_column_page`
    /// MISS — ragged reference lists and dense results columns. Their lines do
    /// not leave the single perfectly-clean empty corridor those detectors
    /// require (long entries occasionally bridge, ragged tails create extra
    /// gaps), so the page currently reads row-major (interleaved). This is the
    /// real-academic M1/M3 deficit.
    ///
    /// Strategy: find the emptiest vertical corridor in the central band, require
    /// it to be near-empty (a genuine gutter), require BALANCED + TALL columns on
    /// both sides (rejects single-column + margin note, and short side captions),
    /// and accept ONLY when both halves classify as reorderable (Prose/Reference)
    /// — so tables, forms, and single-column pages are rejected. Proven on the 5
    /// corpus discriminator PDFs (see `examples/classify_probe.rs`). Returns the
    /// gutter X on accept, else `None` (caller keeps prior behaviour).
    pub(super) fn classifier_column_gutter(spans: &[crate::layout::TextSpan]) -> Option<f32> {
        let finite = |s: &crate::layout::TextSpan| {
            !s.text.trim().is_empty()
                && s.bbox.width > 0.0
                && s.bbox.x.is_finite()
                && s.bbox.width.is_finite()
                && s.bbox.y.is_finite()
        };
        let body: Vec<&crate::layout::TextSpan> = spans.iter().filter(|s| finite(s)).collect();
        if body.len() < 16 {
            return None;
        }
        let cmin = body.iter().map(|s| s.bbox.x).fold(f32::INFINITY, f32::min);
        let cmax = body
            .iter()
            .map(|s| s.bbox.x + s.bbox.width)
            .fold(f32::NEG_INFINITY, f32::max);
        let content_w = cmax - cmin;
        if !content_w.is_finite() || content_w < 100.0 {
            return None;
        }
        let ymin = body.iter().map(|s| s.bbox.y).fold(f32::INFINITY, f32::min);
        let ymax = body
            .iter()
            .map(|s| s.bbox.y)
            .fold(f32::NEG_INFINITY, f32::max);
        let body_h = (ymax - ymin).max(1.0);

        // COLUMN-CONTENT spans = those NOT spanning most of the content width.
        // Full-width spans (titles, the abstract block, section headings, running
        // footers) are BANDS, excluded from gutter detection and classification:
        // counting them would (a) hide the corridor on a mixed page whose top is a
        // full-width title/abstract and bottom is two columns (every paper's
        // page 1), and (b) pollute the per-column class. The corridor, balance,
        // height, and class gates all operate on column-content spans;
        // `reorder_column_major_with_bands` re-emits the bands at their own Y.
        let band_w = 0.6 * content_w;
        let col_idx: Vec<usize> = (0..spans.len())
            .filter(|&i| finite(&spans[i]) && spans[i].bbox.width <= band_w)
            .collect();
        if col_idx.len() < 16 {
            return None;
        }

        // Scan the central band [0.30, 0.70] at fine resolution and find the
        // WIDEST near-empty vertical corridor — the real inter-column gutter —
        // then place the gutter at its midpoint. Picking the widest run (not just
        // any minimal-straddle point) is load-bearing for hanging-indent
        // reference columns: a ragged-ref page has TWO empty corridors — the true
        // gutter between the columns, and a narrow decoy between the right
        // column's hanging entry numbers and its indented text. The decoy is
        // narrower, so the widest-run rule lands the gutter correctly between the
        // columns (otherwise the entry numbers fall into the left column). A
        // single-column body has NO wide empty central corridor (its lines are
        // full-width → excluded above → too few column spans), so it returns None.
        let lo = cmin + 0.30 * content_w;
        let hi = cmin + 0.70 * content_w;
        let step = (content_w / 400.0).clamp(0.5, 3.0);
        // "Empty" tolerates a few stray straddlers (noise / a rare long token).
        let empty_max = (0.01 * col_idx.len() as f32).ceil() as usize;
        let straddle_at = |x: f32| -> usize {
            col_idx
                .iter()
                .filter(|&&i| {
                    spans[i].bbox.x + 2.0 < x && spans[i].bbox.x + spans[i].bbox.width - 2.0 > x
                })
                .count()
        };
        let (mut best_lo, mut best_hi) = (f32::NAN, f32::NAN);
        let (mut run_start, mut in_run, mut best_w) = (lo, false, 0.0f32);
        let mut x = lo;
        while x <= hi {
            if straddle_at(x) <= empty_max {
                if !in_run {
                    run_start = x;
                    in_run = true;
                }
            } else if in_run {
                let w = x - run_start;
                if w > best_w {
                    best_w = w;
                    best_lo = run_start;
                    best_hi = x;
                }
                in_run = false;
            }
            x += step;
        }
        if in_run {
            let w = hi - run_start;
            if w > best_w {
                best_w = w;
                best_lo = run_start;
                best_hi = hi;
            }
        }
        // Require a corridor of real width (a genuine gutter, not a glyph gap).
        if !best_lo.is_finite() || best_w < 6.0 {
            return None;
        }
        let gutter = (best_lo + best_hi) * 0.5;

        // Balanced, tall columns on both sides of the gutter (column-content only).
        let (mut left_idx, mut right_idx): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        let (mut ly0, mut ly1) = (f32::INFINITY, f32::NEG_INFINITY);
        let (mut ry0, mut ry1) = (f32::INFINITY, f32::NEG_INFINITY);
        for &i in &col_idx {
            let s = &spans[i];
            if s.bbox.x + s.bbox.width * 0.5 < gutter {
                left_idx.push(i);
                ly0 = ly0.min(s.bbox.y);
                ly1 = ly1.max(s.bbox.y);
            } else {
                right_idx.push(i);
                ry0 = ry0.min(s.bbox.y);
                ry1 = ry1.max(s.bbox.y);
            }
        }
        let nb = left_idx.len() + right_idx.len();
        if nb == 0 {
            return None;
        }
        // Each side carries a real share of the column content (rejects
        // 1 col + margin note).
        if (left_idx.len() as f32) < 0.30 * nb as f32 || (right_idx.len() as f32) < 0.30 * nb as f32
        {
            return None;
        }
        // Both columns must be tall and of comparable height — they sit BESIDE
        // each other. This rejects a short side-caption/figure-label beside a tall
        // body column, while allowing a mixed page where the columns occupy only
        // the lower portion below a full-width title/abstract (so the floor is
        // 0.4·body_h, not 0.5). `body_h` spans the whole page (title included).
        let (lext, rext) = (ly1 - ly0, ry1 - ry0);
        if lext < 0.4 * body_h || rext < 0.4 * body_h || lext.min(rext) < 0.5 * lext.max(rext) {
            return None;
        }
        // Class gate (load-bearing). NEITHER half may be Table/Form — that is the
        // hard table/form rejection (tables classify Table via mean_chars<10,
        // label/value pages classify Form). AND at least one half must be clearly
        // Prose/Reference, to anchor that this really is a text body. A `Mixed`
        // half is admitted alongside a Prose/Reference half: a dense results
        // column often classifies Mixed (figures, equations, and inline-citation
        // fragments lower its wide-line ratio below the Prose threshold), but it
        // is NOT a table (those are Table, not Mixed), so column-major reading is
        // still correct. Two Mixed halves (no clear prose anchor) stay rejected.
        use crate::layout::RegionClass;
        let lc = crate::layout::classify_region(spans, &left_idx);
        let rc = crate::layout::classify_region(spans, &right_idx);
        let is_table_or_form = |c| matches!(c, RegionClass::Table | RegionClass::Form);
        if is_table_or_form(lc) || is_table_or_form(rc) {
            return None;
        }
        if !(lc.is_reorderable_column() || rc.is_reorderable_column()) {
            return None;
        }
        Some(gutter)
    }
}
