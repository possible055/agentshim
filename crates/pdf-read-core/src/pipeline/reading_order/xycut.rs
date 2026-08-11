//! XY-Cut recursive spatial partitioning for multi-column text layout.
//!
//! This module implements the XY-Cut algorithm per PDF Spec Section 9.4 for
//! recursive geometric analysis without semantic heuristics. Uses projection
//! profiles to detect column boundaries in complex layouts.
//!
//! Per ISO 32000-1:2008:
//! - Section 9.4: Text Objects and coordinates
//! - Section 14.7: Logical Structure (prefers structure tree when available)
//!
//! # Algorithm Overview
//!
//! 1. Compute horizontal projection (white space density across X)
//! 2. Find valleys (gaps) where density < threshold
//! 3. Split region at widest valley (vertical line)
//! 4. Recursively partition left and right sub-regions
//! 5. Alternate to vertical projection if no horizontal valleys found
//! 6. Base case: Sort spans top-to-bottom, left-to-right
//!
//! # Performance
//!
//! Typical newspaper page: ~100 spans, < 5ms processing time
//! Recursive depth: O(log n) for balanced columns

use super::{ReadingOrderContext, ReadingOrderStrategy};
use crate::error::Result;
use crate::geometry::Rect;
use crate::layout::TextSpan;
use crate::pipeline::{OrderedTextSpan, ReadingOrderInfo};

mod columns;
mod partition;
mod projection;

/// Maximum density-array length for XY-cut projection profiles.
///
/// A normal PDF page is at most a few thousand points wide/tall. This limit of
/// 100 000 bins is generous (≈ 33× a 3000-point A0 page) while being small
/// enough to never cause an allocation problem. Spans whose bounding-box span
/// exceeds this limit are the result of a degenerate CTM; returning `None` from
/// the projection safely skips the split instead of attempting a multi-terabyte
/// allocation that would abort the process via `handle_alloc_error`.
const MAX_PROJECTION_SIZE: usize = 100_000;

/// Coarse classification of a region for the #534 multi-column-prose
/// fix. Used to gate the tight-gutter cut: tight cuts are only accepted on
/// regions that *positively* identify as prose, so the same XY-cut recursion
/// no longer corrupts table cells (the lesson — see lines 73–101).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionKind {
    /// Tall stack of wide lines OR tall stack of half-column lines with
    /// substantial content per line. Safe to apply tight-gutter cuts.
    Prose,
    /// Short cells in a grid (mean characters per line < 8). Tight cuts
    /// here corrupt cell ordering — the canonical google_doc population
    /// table that reverted v0.3.53's two attempts is the prototype.
    Table,
    /// Anything else — too few lines, mixed shapes, decorative regions.
    /// Default to the behaviour (no tight cut).
    Mixed,
}

/// Contiguous run of bold-or-larger-font spans spanning ≥ 2 visual lines
/// that the XY-cut splitter must treat as an atomic block. Built by
/// `find_heading_runs` (v0.3.55 #543) BEFORE recursive partitioning,
/// then substituted into the partition input as a single wide synthetic
/// span so cluster-detection / valley-finding can't drive a vertical
/// cut THROUGH a wrapped heading.
///
/// After partition completes, `expand_blocks` projects the synthetic
/// placeholder back into its constituent original spans, preserving
/// each span's per-glyph metadata for downstream consumers
/// (markdown converter heading-level inference, layout-preserving
/// DOCX export, etc.).
#[derive(Debug, Clone)]
struct HeadingRun {
    /// Indices into the original `&[TextSpan]` slice, in reading order
    /// (top-to-bottom, left-to-right within a line).
    span_indices: Vec<usize>,
    /// Union of the constituent spans' bboxes. Substituted for each
    /// individual bbox during partition so the heading appears as one
    /// wide bbox.
    combined_bbox: Rect,
}

/// Union of the bboxes of `spans[indices]`. Empty index list yields a
/// zero-sized rect at the origin (never built in practice — guarded by
/// the caller).
fn union_bboxes(spans: &[TextSpan], indices: &[usize]) -> Rect {
    let mut x_min = f32::MAX;
    let mut y_min = f32::MAX;
    let mut x_max = f32::MIN;
    let mut y_max = f32::MIN;
    for &i in indices {
        let b = spans[i].bbox;
        x_min = x_min.min(b.left());
        x_max = x_max.max(b.right());
        y_min = y_min.min(b.top());
        y_max = y_max.max(b.bottom());
    }
    if x_min == f32::MAX {
        return Rect::default();
    }
    Rect::from_points(x_min, y_min, x_max, y_max)
}

/// XY-Cut recursive spatial partitioning strategy.
///
/// Detects columns using projection profiles and white space analysis.
/// Suitable for newspapers, academic papers, and multi-column layouts.
pub struct XYCutStrategy {
    /// Minimum number of spans in a region before attempting split (default: 5).
    /// Prevents excessive recursion on small regions.
    pub min_spans_for_split: usize,

    /// Valley threshold as fraction of peak projection density (default: 0.3).
    /// Lower values detect narrower gutters, higher values only detect wide gaps.
    pub valley_threshold: f32,

    /// Minimum valley width in points (default: 15.0).
    /// Prevents detecting single-character gaps as column boundaries.
    pub min_valley_width: f32,

    /// Enable horizontal partitioning first, fallback to vertical (default: true).
    ///
    /// Per PDF Spec ISO 32000-1:2008 §14.8.4 (Logical Structure reading order),
    /// column detection is the primary purpose of XY-Cut — horizontal-first
    /// (vertical cut line) splits columns before rows, matching Western
    /// top-down-left-to-right reading order in multi-column documents.
    /// Callers with row-dominant layouts can override via
    /// `with_prefer_horizontal(false)`.
    pub prefer_horizontal: bool,
}

/// Cap on `partition_indexed` recursion depth. Real layouts nest only a few
/// splits deep; this bound only fires on the singleton-peel pathology (many
/// distinct-Y header/footer strips) where unbounded depth is O(n² log n). Set
/// high enough that no real document reaches it.
const MAX_PARTITION_DEPTH: u32 = 64;

impl Default for XYCutStrategy {
    fn default() -> Self {
        Self {
            min_spans_for_split: 5,
            valley_threshold: 0.3,
            // 15pt. Issue #7 (multi-column prose interleaving on
            // issue_07_orphaned_fragments.pdf) was attempted TWICE and
            // REVERTED both times — the 70-PDF sweep caught data
            // corruption in google_doc_document.pdf's population table
            // ("273.879.7501" -> "1273.879.750") each time:
            //
            //   Attempt 1 — lower min_valley_width 15 -> 12 so the tight
            //   ~12pt two-column gutter is detected. Also split the
            //   table's ~12pt inter-cell gaps -> reordered digits.
            //
            //   Attempt 2 — a structural find_two_column_prose_split
            //   (exactly-two recurring left-edge clusters, wide columns,
            //   clean gutter) tried before the single-column check. It
            //   never fired on issue_07's WHOLE page (three left-edge
            //   clusters: full-width intro/footer @60 + left @82 + right
            //   @312, because is_single_column blocks band separation
            //   first), yet it DID fire on a 2-column sub-region of the
            //   google_doc table and reordered cells.
            //
            // Root cause: the same XY-Cut machinery orders both
            // prose-columns and table-cells. Any sensitivity increase
            // that catches issue_07's tight 2-column prose also splits
            // table cells and corrupts data. A correct #7 fix needs a
            // real table-vs-prose classifier (column cells are short
            // values; prose columns are tall stacks of wide lines) AND
            // recursive band-separation of full-width header/footer rows
            // before column detection — a substantial XY-Cut redesign,
            // validated against the full CI corpus, not a local tweak.
            min_valley_width: 15.0,
            prefer_horizontal: true,
        }
    }
}

impl XYCutStrategy {
    /// Create a new XY-Cut strategy with default parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with custom valley threshold (0.0-1.0).
    pub fn with_valley_threshold(mut self, threshold: f32) -> Self {
        self.valley_threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// Create with custom minimum valley width.
    pub fn with_min_valley_width(mut self, width: f32) -> Self {
        self.min_valley_width = width.max(1.0);
        self
    }

    /// Enable or disable horizontal partitioning first preference.
    pub fn with_prefer_horizontal(mut self, prefer: bool) -> Self {
        self.prefer_horizontal = prefer;
        self
    }

    /// Core recursive partitioning algorithm.
    ///
    /// Public for use by MarkdownConverter's ColumnAware reading order mode.
    ///
    /// (#543): runs a pre-pass that detects multi-line heading runs
    /// (bold or larger-than-body font, ≥ 2 wrapped lines with matching
    /// X-extent) and locks them as atomic blocks the recursive splitter
    /// cannot split. Without this, a wrapped heading whose tail lines
    /// Y-overlap with adjacent-column dense content (table caption, table
    /// row, image label) gets bucketed across columns: line 1 glued to the
    /// body paragraph, line 2..N orphaned into the wrong block — and the
    /// markdown converter then promotes the orphan tail to a phantom
    /// heading (`### …`) in the wrong location.
    pub fn partition_region(&self, spans: &[TextSpan]) -> Vec<Vec<TextSpan>> {
        let heading_runs = self.find_heading_runs(spans);
        if heading_runs.is_empty() {
            // Hot path: no headings found, skip the synthesize/expand
            // pair entirely so the cost is bounded to one O(n log n) sort
            // inside find_heading_runs.
            let indices: Vec<usize> = (0..spans.len()).collect();
            let index_groups = self.partition_indexed(spans, &indices);
            return index_groups
                .into_iter()
                .map(|group| group.into_iter().map(|i| spans[i].clone()).collect())
                .collect();
        }

        // Build synthetic span list: each heading run collapses to ONE
        // wide span carrying the union bbox; non-heading spans pass
        // through unchanged. The synthetic list is shorter than the
        // original by (sum_of_run_sizes - num_runs).
        let (synthetic, synthetic_origin) = self.synthesize_for_partition(spans, &heading_runs);
        let synth_indices: Vec<usize> = (0..synthetic.len()).collect();
        let synth_groups = self.partition_indexed(&synthetic, &synth_indices);

        // Project synthetic-space groups back into original-span space:
        // each synthetic span that came from a heading run gets expanded
        // back into its constituent original spans (in their original
        // reading-order sequence within the run).
        self.expand_blocks(synth_groups, spans, &synthetic_origin)
    }

    /// Detect contiguous bold/large-font runs that span ≥ 2 lines with
    /// matching X-extent (i.e. wrapped subsection headings).
    ///
    /// Per the fix-543 plan §A.2: two adjacent spans (in reading
    /// order) are considered to belong to the same heading run when
    /// ALL of the following hold:
    ///
    /// 1. Both are heading-like (bold, OR font_size > median × 1.15).
    /// 2. Same font_size (within 0.5 pt epsilon).
    /// 3. Same bold flag.
    /// 4. Next span's left edge is within `[prev.left, prev.left + 6pt]`
    ///    (wrapped heading lines often re-indent by up to ~6pt).
    /// 5. Next span sits ≤ 1.5 × line-height below the previous span
    ///    (a single-line gap; double-line gaps are paragraph breaks).
    ///
    /// `median_font_size` is computed across non-bold spans so heavy
    /// bold runs don't bias the body-size estimate upward.
    fn find_heading_runs(&self, spans: &[TextSpan]) -> Vec<HeadingRun> {
        if spans.len() < 2 {
            return Vec::new();
        }

        // Median body font size from NON-bold spans only. Bold spans
        // typically sit at heading sizes (bigger than body), so including
        // them biases the median high and we'd miss bold headings whose
        // size sits between body and the heavier weight tier.
        let mut non_bold_sizes: Vec<f32> = spans
            .iter()
            .filter(|s| !s.font_weight.is_bold())
            .map(|s| s.font_size)
            .filter(|&sz| sz > 0.0)
            .collect();
        let median_body = if non_bold_sizes.is_empty() {
            // Fallback: all spans bold (or zero-size). Use overall median.
            let mut sizes: Vec<f32> = spans
                .iter()
                .map(|s| s.font_size)
                .filter(|&sz| sz > 0.0)
                .collect();
            if sizes.is_empty() {
                return Vec::new();
            }
            sizes.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
            sizes[sizes.len() / 2]
        } else {
            non_bold_sizes.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
            non_bold_sizes[non_bold_sizes.len() / 2]
        };
        let heading_size_floor = median_body * 1.15;

        let is_heading_like =
            |s: &TextSpan| -> bool { s.font_weight.is_bold() || s.font_size > heading_size_floor };

        // Sort indices by reading order (top of page first; Rect::top()
        // is the SMALLER Y of the normalized rect — see comment at
        // line ~885 — so larger Y = higher on page in PDF coords;
        // we want DESCENDING Y here).
        let mut order: Vec<usize> = (0..spans.len()).collect();
        order.sort_by(|&a, &b| {
            let y_cmp = crate::utils::safe_float_cmp(spans[b].bbox.top(), spans[a].bbox.top());
            if y_cmp != std::cmp::Ordering::Equal {
                return y_cmp;
            }
            crate::utils::safe_float_cmp(spans[a].bbox.left(), spans[b].bbox.left())
        });

        // Cluster reading-order-adjacent heading-like spans into runs.
        // The same line may carry multiple bold spans (one per Tj
        // segment); we collapse runs across lines, not within a line.
        let indent_tolerance = 6.0_f32;
        let font_eps = 0.5_f32;
        let mut runs: Vec<Vec<usize>> = Vec::new();
        let mut current: Vec<usize> = Vec::new();

        for &idx in &order {
            let span = &spans[idx];
            if !is_heading_like(span) {
                if !current.is_empty() {
                    runs.push(std::mem::take(&mut current));
                }
                continue;
            }

            if current.is_empty() {
                current.push(idx);
                continue;
            }

            let last_idx = *current.last().unwrap();
            let last = &spans[last_idx];

            // (2) same font size, (3) same bold flag.
            let size_ok = (span.font_size - last.font_size).abs() <= font_eps;
            let bold_ok = span.font_weight.is_bold() == last.font_weight.is_bold();

            // Same-line: top within 1 pt of last's top — fold without
            // applying indent/leading checks (both spans belong to the
            // SAME wrapped-heading line, e.g. two bold Tj segments).
            let same_line = (span.bbox.top() - last.bbox.top()).abs() <= 1.0;

            if size_ok && bold_ok && same_line {
                current.push(idx);
                continue;
            }

            // Different line: enforce indent (4) + leading (5).
            // line_height = max of the two spans' bbox heights, plus a
            // floor of font_size to handle ascender-only / descender-only
            // glyphs with collapsed bboxes.
            let line_h = last
                .bbox
                .height
                .max(span.bbox.height)
                .max(last.font_size)
                .max(1.0);
            let leading_tolerance = line_h * 1.5;

            // PDF coords: y grows up, so the wrapped line sits at a
            // SMALLER bbox.top than the previous line. The gap between
            // last's bottom and span's top should fit inside the leading
            // tolerance.
            let last_bottom = last.bbox.top(); // smaller-Y edge in PDF coords (see find_vertical_split comment)
            let span_top = span.bbox.top();
            let vertical_gap = (last_bottom - span_top).abs();

            let indent_ok = span.bbox.left() >= last.bbox.left() - indent_tolerance
                && span.bbox.left() <= last.bbox.left() + indent_tolerance;
            let leading_ok = vertical_gap <= leading_tolerance;

            if size_ok && bold_ok && indent_ok && leading_ok {
                current.push(idx);
            } else {
                runs.push(std::mem::take(&mut current));
                current.push(idx);
            }
        }
        if !current.is_empty() {
            runs.push(current);
        }

        // A run becomes a HeadingRun only when it spans ≥ 2 distinct
        // lines. Single-line bold spans (inline emphasis, lone short
        // headings) don't need locking — XY-cut handles them correctly
        // already, and locking them would be a no-op for the splitter
        // but adds overhead.
        runs.into_iter()
            .filter_map(|span_indices| {
                if span_indices.len() < 2 {
                    return None;
                }
                let mut distinct_lines = std::collections::BTreeSet::new();
                for &i in &span_indices {
                    distinct_lines.insert(spans[i].bbox.top().round() as i32);
                }
                if distinct_lines.len() < 2 {
                    return None;
                }
                Some(HeadingRun {
                    combined_bbox: union_bboxes(spans, &span_indices),
                    span_indices,
                })
            })
            .collect()
    }

    /// Build a synthetic span list where each detected `HeadingRun`
    /// collapses to ONE wide synthetic span carrying the union bbox.
    /// Non-heading spans pass through unchanged.
    ///
    /// Returns:
    /// - `synthetic`: the input to `partition_indexed`.
    /// - `synthetic_origin[k]`: indices of ORIGINAL spans backing
    ///   synthetic span `k`. Length 1 for pass-throughs, ≥ 2 for
    ///   heading-run placeholders. Used by `expand_blocks` to project
    ///   partition output back into original-span space.
    fn synthesize_for_partition(
        &self,
        spans: &[TextSpan],
        runs: &[HeadingRun],
    ) -> (Vec<TextSpan>, Vec<Vec<usize>>) {
        // Mark each original span with the heading-run it belongs to
        // (or None for pass-through).
        let mut in_run: Vec<Option<usize>> = vec![None; spans.len()];
        for (r_idx, run) in runs.iter().enumerate() {
            for &i in &run.span_indices {
                in_run[i] = Some(r_idx);
            }
        }

        let mut synthetic: Vec<TextSpan> = Vec::with_capacity(spans.len());
        let mut origins: Vec<Vec<usize>> = Vec::with_capacity(spans.len());
        let mut emitted_run = vec![false; runs.len()];

        for (i, span) in spans.iter().enumerate() {
            match in_run[i] {
                None => {
                    synthetic.push(span.clone());
                    origins.push(vec![i]);
                }
                Some(r_idx) if !emitted_run[r_idx] => {
                    // Emit the run as a synthetic placeholder at the
                    // position of its first-encountered span.
                    let run = &runs[r_idx];
                    let mut placeholder = span.clone();
                    placeholder.bbox = run.combined_bbox;
                    // Concatenate the run's text with single spaces so
                    // is_single_column_region's core-width estimate is
                    // proportional to the actual heading length, not the
                    // single first-line fragment.
                    let mut combined_text = String::new();
                    for (k, &si) in run.span_indices.iter().enumerate() {
                        if k > 0 {
                            combined_text.push(' ');
                        }
                        combined_text.push_str(&spans[si].text);
                    }
                    placeholder.text = combined_text;
                    synthetic.push(placeholder);
                    origins.push(run.span_indices.clone());
                    emitted_run[r_idx] = true;
                }
                Some(_) => { /* already emitted — skip later spans of the run */ }
            }
        }

        (synthetic, origins)
    }

    /// Project partition groups from synthetic-span space back into
    /// original-span space, expanding each heading-run placeholder into
    /// its constituent original spans (in their original ordering).
    fn expand_blocks(
        &self,
        synth_groups: Vec<Vec<usize>>,
        original: &[TextSpan],
        synthetic_origin: &[Vec<usize>],
    ) -> Vec<Vec<TextSpan>> {
        synth_groups
            .into_iter()
            .map(|group| {
                let mut out = Vec::with_capacity(group.len());
                for synth_idx in group {
                    for &orig_idx in &synthetic_origin[synth_idx] {
                        out.push(original[orig_idx].clone());
                    }
                }
                out
            })
            .collect()
    }
}

/// Internal projection profile representation.
struct ProjectionProfile {
    /// Density values (height or width accumulated per bin)
    density: Vec<f32>,

    /// Origin coordinates
    x_min: f32,
    y_min: f32,
}

impl ReadingOrderStrategy for XYCutStrategy {
    fn apply(
        &self,
        spans: Vec<TextSpan>,
        _context: &ReadingOrderContext,
    ) -> Result<Vec<OrderedTextSpan>> {
        // (#543): detect multi-line heading runs and route the
        // partition through synthetic-span space so the splitter treats
        // each wrapped heading as a single atomic block. When no
        // headings are found we use the original index-only path that
        // avoids span clones during recursion.
        let heading_runs = self.find_heading_runs(&spans);

        let index_groups: Vec<Vec<usize>> = if heading_runs.is_empty() {
            let indices: Vec<usize> = (0..spans.len()).collect();
            self.partition_indexed(&spans, &indices)
        } else {
            let (synthetic, synthetic_origin) =
                self.synthesize_for_partition(&spans, &heading_runs);
            let synth_indices: Vec<usize> = (0..synthetic.len()).collect();
            let synth_groups = self.partition_indexed(&synthetic, &synth_indices);
            // Project synthetic-space groups back to ORIGINAL-span
            // indices (so the move-out below works on the input Vec).
            synth_groups
                .into_iter()
                .map(|group| {
                    let mut out = Vec::with_capacity(group.len());
                    for synth_idx in group {
                        out.extend(synthetic_origin[synth_idx].iter().copied());
                    }
                    out
                })
                .collect()
        };

        // Build result — moves spans out by index (no extra clone)
        let mut ordered = Vec::with_capacity(spans.len());
        // Convert spans to indexable storage for O(1) moves
        let mut span_slots: Vec<Option<TextSpan>> = spans.into_iter().map(Some).collect();
        let mut order_index = 0usize;

        for (group_idx, group) in index_groups.iter().enumerate() {
            for &i in group {
                if let Some(span) = span_slots[i].take() {
                    ordered.push(
                        OrderedTextSpan::with_info(span, order_index, ReadingOrderInfo::xycut())
                            .with_group(group_idx),
                    );
                    order_index += 1;
                }
            }
        }

        Ok(ordered)
    }

    fn name(&self) -> &'static str {
        "XYCutStrategy"
    }
}
