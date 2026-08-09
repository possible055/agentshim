// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::enum_variant_names)]
#![allow(clippy::wrong_self_convention)]
#![allow(clippy::explicit_counter_loop)]
#![allow(clippy::doc_overindented_list_items)]
#![allow(clippy::should_implement_trait)]
#![allow(clippy::redundant_guards)]
#![allow(clippy::regex_creation_in_loops)]
#![allow(clippy::manual_find)]
#![allow(clippy::match_like_matches_macro)]
#![allow(clippy::collapsible_match)]
#![cfg_attr(test, allow(dead_code, unused_variables))]

//! Read-only PDF parsing, Markdown extraction, page classification, and raster rendering for
//! `codexshim`.
//!
//! The supported interface is [`PdfReadDocument`]. The implementation is a selective derivative
//! of `pdf_oxide`; provenance and the local patch policy are recorded in `UPSTREAM.md`.
#![warn(missing_docs)]
// Glibc 2.34 compatibility (#416): LLVM may emit calls to __memcmpeq@GLIBC_2.35,
// which does not exist in glibc 2.34 (Amazon Linux 2023, some Ubuntu 22.04 builds).
// A weak stub redirecting to plain memcmp satisfies the reference on older glibc;
// glibc 2.35's own definition wins when available. global_asm! works with both
// GNU ld and lld, unlike --defsym which lld rejects for PLT-resolved symbols.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
core::arch::global_asm!(
    ".weak __memcmpeq",
    ".type __memcmpeq, @function",
    "__memcmpeq:",
    "jmp memcmp@PLT",
);

mod error;
mod read_api;

pub use read_api::{
    MarkdownOptions, PageClass, PageInfo, ParserLimits, PdfReadDocument, PdfReadError,
    PdfReadErrorKind, RenderLimits, RenderedPage,
};

pub(crate) mod cache;

mod document;
mod lexer;
mod object;
mod objstm;
mod parser;
mod parser_config;
mod xref;
mod xref_reconstruction;

mod decoders;

mod color;
mod crypto;
mod encryption;
mod functions;

mod geometry;
mod layout;

mod content;
mod extractors;
mod fonts;
mod optional_content;
mod text;

mod annotation_types;
mod annotations;
mod config;
mod converters;
mod elements;
mod outline;
mod pipeline;
mod structure;

#[cfg(feature = "rendering")]
mod rendering;

pub(crate) use document::{ExtractedImageRef, ImageFormat, PdfDocument, ReadingOrder};
pub(crate) use error::{Error, Result};
pub(crate) use pipeline::XYCutStrategy;

pub(crate) mod utils {
    //! Internal utility functions for the library.

    use std::cmp::Ordering;

    /// Safely truncate a string to at most `max_bytes` from the start
    /// without splitting a multi-byte UTF-8 character.
    ///
    /// Returns the full string if it is shorter than `max_bytes`.
    /// When truncation lands inside a multi-byte character, the boundary
    /// is rounded **down** to the nearest char boundary (floor).
    #[inline]
    pub fn safe_prefix(s: &str, max_bytes: usize) -> &str {
        if s.len() <= max_bytes {
            return s;
        }
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }

    /// Safely take the last `max_bytes` of a string without splitting
    /// a multi-byte UTF-8 character.
    ///
    /// Returns the full string if it is shorter than `max_bytes`.
    /// When the computed start offset lands inside a multi-byte character,
    /// the boundary is rounded **up** to the nearest char boundary (ceil).
    #[inline]
    pub fn safe_suffix(s: &str, max_bytes: usize) -> &str {
        if s.len() <= max_bytes {
            return s;
        }
        let start = s.len() - max_bytes;
        let mut safe_start = start;
        while safe_start < s.len() && !s.is_char_boundary(safe_start) {
            safe_start += 1;
        }
        &s[safe_start..]
    }

    /// Y-band tolerance used by `row_aware_span_cmp`.
    ///
    /// Two spans whose top-Y differs by less than this amount are treated
    /// as lying on the same row. Chosen to absorb typographic baseline
    /// jitter for 10-12pt body text and glyph-cluster offsets in CJK
    /// fonts without merging adjacent 14pt-leading lines.
    pub const ROW_BAND_TOLERANCE_PT: f32 = 3.0;

    /// Row-aware reading-order comparator for spans.
    ///
    /// Sorts primarily by "row band" (top-Y quantized to
    /// `ROW_BAND_TOLERANCE_PT`, larger Y first per PDF Spec ISO 32000-1:2008
    /// §8.3.2.3) and secondarily by X (left-to-right within a row). This
    /// keeps tabular layouts where cells in the same logical row have
    /// slightly different Y values (font-metric jitter, superscripts, CJK
    /// glyph centering) from being interleaved by a strict Y sort.
    ///
    /// Uses `i32` band keys so the ordering is a valid total order —
    /// comparing raw Y values with tolerance is non-transitive and would
    /// break `sort_by`.
    #[inline]
    pub fn row_aware_span_cmp(a_y: f32, a_x: f32, b_y: f32, b_x: f32) -> Ordering {
        // Non-finite Y (NaN/±Inf) cannot be quantized into an i32 band —
        // `as i32` saturates, collapsing distinct non-finite values into
        // the same band and reordering them unpredictably against finite
        // spans. Fall back to `safe_float_cmp` so non-finite values follow
        // the same NaN-last / total-order policy used everywhere else.
        if !a_y.is_finite() || !b_y.is_finite() {
            return safe_float_cmp(b_y, a_y).then_with(|| safe_float_cmp(a_x, b_x));
        }
        let band_a = (a_y / ROW_BAND_TOLERANCE_PT).round() as i32;
        let band_b = (b_y / ROW_BAND_TOLERANCE_PT).round() as i32;
        // Larger Y = higher on page → descending band order.
        match band_b.cmp(&band_a) {
            Ordering::Equal => safe_float_cmp(a_x, b_x),
            other => other,
        }
    }

    /// Dominant text-matrix rotation of a page's spans, if any.
    ///
    /// Returns the snapped rotation (`90` / `180` / `-90`) shared by at
    /// least half of the page's non-whitespace spans, or `None` when the
    /// page is predominantly upright (or empty). The half-or-more majority
    /// mirrors the existing vertical-CJK (tategaki) vote: at most one
    /// rotation group can dominate, and a marginal stamp or figure label
    /// can never hijack the page frame. Rotations are grouped with the same
    /// 0.5° tolerance `order_rotated_blocks` uses, so free-angle (skewed)
    /// text never forms a quadrant group.
    pub(crate) fn dominant_rotation(spans: &[crate::layout::TextSpan]) -> Option<f32> {
        let mut groups: Vec<(f32, usize)> = Vec::new();
        let mut total = 0usize;
        for s in spans {
            if s.text.trim().is_empty() {
                continue;
            }
            total += 1;
            if s.rotation_degrees == 0.0 {
                continue;
            }
            match groups
                .iter_mut()
                .find(|(k, _)| (*k - s.rotation_degrees).abs() < 0.5)
            {
                Some(g) => g.1 += 1,
                None => groups.push((s.rotation_degrees, 1)),
            }
        }
        groups
            .into_iter()
            .max_by_key(|&(_, n)| n)
            .filter(|&(_, n)| n * 2 >= total && total > 0)
            .map(|(deg, _)| deg)
    }

    /// Right-to-left variant of [`row_aware_span_cmp`] (issues #656/#657).
    ///
    /// Identical row banding (lines top-to-bottom), but orders spans
    /// **right-to-left within a row** (X descending). A pure-RTL line's
    /// logical reading order *is* its rightmost-first geometric order, so
    /// sorting word-spans by descending X reconstructs logical order
    /// directly from page geometry — independent of whether the producer
    /// stored the run in visual or logical order. Used by the tagged
    /// struct-tree assemblers, which otherwise have no span-order pass for
    /// RTL (the untagged `reverse_rtl_visual_order_runs` is never reached
    /// on tagged pages).
    ///
    /// Retained as a tested geometric utility: the tagged RTL assembler now
    /// orders pure-RTL spans via `document::PdfDocument::order_pure_rtl_spans`
    /// (font-relative line grouping), which subsumes the fixed-band comparator,
    /// so this has no production caller at present.
    #[inline]
    #[allow(dead_code)]
    pub fn row_aware_span_cmp_rtl(a_y: f32, a_x: f32, b_y: f32, b_x: f32) -> Ordering {
        if !a_y.is_finite() || !b_y.is_finite() {
            return safe_float_cmp(b_y, a_y).then_with(|| safe_float_cmp(b_x, a_x));
        }
        let band_a = (a_y / ROW_BAND_TOLERANCE_PT).round() as i32;
        let band_b = (b_y / ROW_BAND_TOLERANCE_PT).round() as i32;
        match band_b.cmp(&band_a) {
            Ordering::Equal => safe_float_cmp(b_x, a_x), // X descending = RTL
            other => other,
        }
    }

    /// Sort spans into tategaki (vertical-writing) reading order:
    /// right-to-left across columns, top-to-bottom within each column (PDF
    /// user-space Y increases upward, so top-first means Y descending).
    ///
    /// Columns are found by single-linkage clustering of X-centers: order
    /// the centers right-to-left, then start a new column whenever the gap
    /// to the previous center exceeds `tol` (the median span width —
    /// tategaki CJK body text is functionally monospaced, so this
    /// approximates the column pitch: wide enough to keep one column
    /// together, narrow enough to separate the next).
    ///
    /// Comparing raw X-centers against a `|a - b| <= tol` tolerance
    /// *inside* a sort comparator is not transitive — a chain of spans
    /// each within `tol` of its neighbor can span far more than `tol`
    /// overall, so "same column" isn't an equivalence relation and
    /// `sort_by` can panic with "does not correctly implement a total
    /// order". Clustering into columns first and sorting by `(column, Y)`
    /// avoids this: every comparison is between two discrete, precomputed
    /// keys, which is transitive by construction. It's also more accurate
    /// than quantizing each X-center into a fixed-size band independently
    /// (e.g. `round(x / tol)`) — banding can split two spans that are only
    /// a couple points apart into different buckets if they straddle a
    /// bucket boundary, even though they're well within `tol` of each
    /// other; single-linkage clustering only looks at the gap between
    /// neighbors, so it has no such boundary effect.
    pub fn sort_vertical_tategaki<T>(
        items: Vec<T>,
        get_bbox: impl Fn(&T) -> &crate::geometry::Rect,
    ) -> Vec<T> {
        if items.len() < 2 {
            return items;
        }

        let mut widths: Vec<f32> = items.iter().map(|it| get_bbox(it).width.max(1.0)).collect();
        widths.sort_by(|a, b| safe_float_cmp(*a, *b));
        let tol = widths[widths.len() / 2].max(1.0);

        let centers: Vec<f32> = items
            .iter()
            .map(|it| {
                let b = get_bbox(it);
                b.x + b.width * 0.5
            })
            .collect();
        let ys: Vec<f32> = items.iter().map(|it| get_bbox(it).y).collect();

        // Right-to-left pass assigning column ids. Stable sort keeps ties
        // in input order, so clustering is deterministic.
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by(|&a, &b| safe_float_cmp(centers[b], centers[a]));

        let mut column = vec![0u32; items.len()];
        let mut current = 0u32;
        let mut prev = centers[order[0]];
        for &idx in &order[1..] {
            let center = centers[idx];
            // A NaN gap (either end non-finite) never chains, so a
            // non-finite center always starts its own column.
            let gap = prev - center;
            if gap.is_nan() || gap > tol {
                current += 1;
            }
            column[idx] = current;
            prev = center;
        }

        // Column ascending (columns were numbered right-to-left above),
        // then top-to-bottom within a column. Both keys are total orders.
        order.sort_by(|&a, &b| {
            column[a]
                .cmp(&column[b])
                .then_with(|| safe_float_cmp(ys[b], ys[a]))
        });

        let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
        order
            .into_iter()
            .map(|i| slots[i].take().expect("each index appears once"))
            .collect()
    }

    /// Safely compare two floating point numbers, handling NaN cases.
    ///
    /// NaN values are treated as equal to each other and greater than all other values.
    /// This ensures that sorting operations never panic due to NaN comparisons.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// # use std::cmp::Ordering;
    /// # use pdf_oxide::utils::safe_float_cmp;
    /// assert_eq!(safe_float_cmp(1.0, 2.0), Ordering::Less);
    /// assert_eq!(safe_float_cmp(2.0, 1.0), Ordering::Greater);
    /// assert_eq!(safe_float_cmp(1.0, 1.0), Ordering::Equal);
    ///
    /// // NaN handling
    /// assert_eq!(safe_float_cmp(f32::NAN, f32::NAN), Ordering::Equal);
    /// assert_eq!(safe_float_cmp(f32::NAN, 1.0), Ordering::Greater);
    /// assert_eq!(safe_float_cmp(1.0, f32::NAN), Ordering::Less);
    /// ```
    #[inline]
    pub fn safe_float_cmp(a: f32, b: f32) -> Ordering {
        match (a.is_nan(), b.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater, // NaN > all numbers
            (false, true) => Ordering::Less,    // all numbers < NaN
            (false, false) => {
                // Both are normal numbers, safe to unwrap
                a.partial_cmp(&b).unwrap()
            }
        }
    }

    /// Sort `items` into row-band reading order, computing each element's band
    /// key once instead of re-quantizing on every `row_aware_span_cmp`
    /// comparison.
    ///
    /// When all `y`/`x` are finite this is a cached-key stable sort with the
    /// same order as `sort_by(row_aware_span_cmp)` (band descending, then `x`
    /// ascending — `f32::total_cmp` equals `safe_float_cmp` for finite values,
    /// and both are stable on ties). Otherwise it falls back to the comparator
    /// so the NaN/±∞ policy is unchanged.
    pub fn sort_by_row_band<T>(
        items: &mut [T],
        get_y: impl Fn(&T) -> f32,
        get_x: impl Fn(&T) -> f32,
    ) {
        let all_finite = items
            .iter()
            .all(|it| get_y(it).is_finite() && get_x(it).is_finite());
        if !all_finite {
            items.sort_by(|a, b| row_aware_span_cmp(get_y(a), get_x(a), get_y(b), get_x(b)));
            return;
        }
        // Cached-key stable sort. `total_cmp` matches `safe_float_cmp` for the
        // finite values we gated on above.
        items.sort_by_cached_key(|it| {
            let band = (get_y(it) / ROW_BAND_TOLERANCE_PT).round() as i32;
            // Reverse band → larger Y (higher on page) first, matching the
            // comparator's `band_b.cmp(&band_a)`.
            (std::cmp::Reverse(band), F32Ord(get_x(it)))
        });
    }

    /// Total-order wrapper over `f32` for use as a sort key. For finite values
    /// `total_cmp` is identical to `safe_float_cmp` / `partial_cmp`.
    #[derive(Clone, Copy, PartialEq)]
    struct F32Ord(f32);
    impl Eq for F32Ord {}
    impl PartialOrd for F32Ord {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for F32Ord {
        fn cmp(&self, other: &Self) -> Ordering {
            self.0.total_cmp(&other.0)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The cached-key sort must produce the identical permutation to
        /// `sort_by(row_aware_span_cmp)` on finite inputs.
        #[test]
        fn test_sort_by_row_band_matches_comparator() {
            // Deterministic pseudo-random spans (no rng in tests).
            let raw: Vec<(f32, f32)> = (0..500)
                .map(|i| {
                    let y = ((i * 37 % 113) as f32) * 1.3;
                    let x = ((i * 71 % 97) as f32) * 2.1;
                    (y, x)
                })
                .collect();
            let mut a = raw.clone();
            let mut b = raw.clone();
            sort_by_row_band(&mut a, |t| t.0, |t| t.1);
            b.sort_by(|p, q| row_aware_span_cmp(p.0, p.1, q.0, q.1));
            assert_eq!(
                a, b,
                "cached-key sort must match the comparator permutation"
            );
        }

        #[test]
        fn test_safe_float_cmp_normal() {
            assert_eq!(safe_float_cmp(1.0, 2.0), Ordering::Less);
            assert_eq!(safe_float_cmp(2.0, 1.0), Ordering::Greater);
            assert_eq!(safe_float_cmp(1.5, 1.5), Ordering::Equal);
        }

        #[test]
        fn test_safe_float_cmp_nan() {
            assert_eq!(safe_float_cmp(f32::NAN, f32::NAN), Ordering::Equal);
            assert_eq!(safe_float_cmp(f32::NAN, 0.0), Ordering::Greater);
            assert_eq!(safe_float_cmp(0.0, f32::NAN), Ordering::Less);
        }

        fn tategaki_rect(x: f32, y: f32, w: f32) -> crate::geometry::Rect {
            crate::geometry::Rect::new(x, y, w, 12.0)
        }

        /// Two well-separated columns: rightmost column first, top-to-bottom
        /// within each (the ordering the pre-fix comparator also produced
        /// for the well-behaved case — this must not regress).
        #[test]
        fn test_sort_vertical_tategaki_two_columns() {
            let items = vec![
                ("D", tategaki_rect(300.0, 700.0, 12.0)),
                ("F", tategaki_rect(300.0, 676.0, 12.0)),
                ("B", tategaki_rect(500.0, 688.0, 12.0)),
                ("C", tategaki_rect(500.0, 676.0, 12.0)),
                ("A", tategaki_rect(500.0, 700.0, 12.0)),
                ("E", tategaki_rect(300.0, 688.0, 12.0)),
            ];
            let sorted = sort_vertical_tategaki(items, |it| &it.1);
            let order: String = sorted.iter().map(|it| it.0).collect();
            assert_eq!(order, "ABCDEF");
        }

        /// A chain of X-centers each within `tol` of its neighbor but
        /// spanning far more than `tol` overall made the old pairwise
        /// `|a - b| <= tol` comparator non-transitive (A<B, B<C, C<A),
        /// which panicked `sort_by` on Rust 1.81+. Single-linkage
        /// clustering must read the whole chain as one column, top to
        /// bottom, without panicking.
        #[test]
        fn test_sort_vertical_tategaki_chained_centers() {
            // Centers step by 8pt across 64 spans (630pt total span) — every
            // adjacent pair is "same column" under a naive tolerance check,
            // but the first and last are 500+pt apart.
            let items: Vec<(usize, crate::geometry::Rect)> = (0..64)
                .map(|i| {
                    (
                        i,
                        tategaki_rect(i as f32 * 8.0, ((i * 37) % 64) as f32 * 7.0, 10.0),
                    )
                })
                .collect();
            let sorted = sort_vertical_tategaki(items, |it| &it.1);
            assert_eq!(sorted.len(), 64);
            assert!(
                sorted.windows(2).all(|w| w[0].1.y >= w[1].1.y),
                "one chained column must read top-to-bottom"
            );
        }

        /// Two spans only 2pt apart (well within `tol`) must land in the
        /// same column even when their absolute X-centers straddle what
        /// would be a fixed quantization-bucket boundary (e.g. `tol`
        /// multiples of 100 straddling x=250). Single-linkage clustering
        /// only looks at the gap between neighbors, so it has no such
        /// boundary effect — unlike banding each center independently via
        /// `round(x / tol)`.
        #[test]
        fn test_sort_vertical_tategaki_no_boundary_straddle_effect() {
            let items = vec![
                ("near", tategaki_rect(249.0, 700.0, 100.0)),
                ("straddle", tategaki_rect(251.0, 690.0, 100.0)),
                ("far", tategaki_rect(10.0, 680.0, 100.0)),
            ];
            let sorted = sort_vertical_tategaki(items, |it| &it.1);
            // "near" and "straddle" are 2pt apart (tol = 100) so they must
            // share a column and sort top-to-bottom relative to each other,
            // both ahead of the genuinely distant "far" column.
            let order: Vec<&str> = sorted.iter().map(|it| it.0).collect();
            assert_eq!(order, vec!["near", "straddle", "far"]);
        }

        /// Non-finite coordinates must not panic the sort, and every item
        /// must survive the permutation exactly once.
        #[test]
        fn test_sort_vertical_tategaki_non_finite() {
            let mut items: Vec<(usize, crate::geometry::Rect)> = (0..32)
                .map(|i| {
                    (
                        i,
                        tategaki_rect((i % 8) as f32 * 40.0, i as f32 * 5.0, 12.0),
                    )
                })
                .collect();
            items[3].1.x = f32::NAN;
            items[11].1.y = f32::NAN;
            items[17].1.width = f32::NAN;
            items[23].1.x = f32::INFINITY;
            let sorted = sort_vertical_tategaki(items, |it| &it.1);
            let mut ids: Vec<usize> = sorted.iter().map(|it| it.0).collect();
            ids.sort_unstable();
            assert_eq!(ids, (0..32).collect::<Vec<_>>());
        }

        #[test]
        fn test_safe_float_cmp_infinity() {
            assert_eq!(
                safe_float_cmp(f32::INFINITY, f32::INFINITY),
                Ordering::Equal
            );
            assert_eq!(safe_float_cmp(f32::INFINITY, 1.0), Ordering::Greater);
            assert_eq!(
                safe_float_cmp(f32::NEG_INFINITY, f32::INFINITY),
                Ordering::Less
            );
        }

        /// Verify that sort_by using safe_float_cmp never panics with NaN values.
        /// This is a regression test for the "total order" panic that affected 42
        /// PDFs across 5 test datasets (issue found in v0.3.11-pre).
        #[test]
        fn test_sort_with_nan_does_not_panic() {
            let mut values = [3.0_f32, f32::NAN, 1.0, f32::NAN, 2.0, f32::NAN, 0.5];
            values.sort_by(|a, b| safe_float_cmp(*a, *b));
            // NaN values should sort to the end (NaN > all numbers)
            assert!(values[0..4].iter().all(|v| !v.is_nan()));
            assert!(values[4..].iter().all(|v| v.is_nan()));
        }

        /// Verify transitivity: if a < b and b < c then a < c.
        /// The previous `partial_cmp().unwrap_or(Equal)` pattern violated this
        /// when NaN was involved, causing Rust's sort to panic.
        #[test]
        fn test_safe_float_cmp_transitivity() {
            let a = 1.0_f32;
            let b = 2.0_f32;
            let nan = f32::NAN;

            // a < b
            assert_eq!(safe_float_cmp(a, b), Ordering::Less);
            // b < NaN
            assert_eq!(safe_float_cmp(b, nan), Ordering::Less);
            // Therefore a < NaN (transitivity)
            assert_eq!(safe_float_cmp(a, nan), Ordering::Less);
        }

        /// Cells in the same tabular row with slightly-different Y values
        /// must stay together and be ordered by X, not interleaved with
        /// cells from other rows.
        #[test]
        fn test_row_aware_span_cmp_tolerates_y_jitter() {
            // Row 1 at y ≈ 100 with small per-cell jitter.
            // Row 2 at y ≈ 86 (14pt leading below).
            // A strict Y sort would interleave them because some row-1
            // cells have lower Y than some row-2 cells.
            #[derive(Debug, Clone, Copy)]
            struct Cell {
                y: f32,
                x: f32,
                id: &'static str,
            }
            let mut cells = [
                Cell {
                    y: 100.5,
                    x: 50.0,
                    id: "r1-c1",
                },
                Cell {
                    y: 99.7,
                    x: 150.0,
                    id: "r1-c2",
                },
                Cell {
                    y: 100.2,
                    x: 250.0,
                    id: "r1-c3",
                },
                Cell {
                    y: 86.4,
                    x: 50.0,
                    id: "r2-c1",
                },
                Cell {
                    y: 85.8,
                    x: 150.0,
                    id: "r2-c2",
                },
                Cell {
                    y: 86.1,
                    x: 250.0,
                    id: "r2-c3",
                },
            ];
            cells.sort_by(|a, b| row_aware_span_cmp(a.y, a.x, b.y, b.x));
            let order: Vec<&str> = cells.iter().map(|c| c.id).collect();
            assert_eq!(
                order,
                vec!["r1-c1", "r1-c2", "r1-c3", "r2-c1", "r2-c2", "r2-c3"],
                "cells from the same row must stay contiguous and X-sorted"
            );
        }

        /// Row-aware comparator must still put distinct-leading rows in
        /// top-to-bottom reading order.
        #[test]
        fn test_row_aware_span_cmp_distinct_rows_descending() {
            let mut rows = [
                (100.0f32, 0.0f32, "top"),
                (50.0, 0.0, "middle"),
                (10.0, 0.0, "bottom"),
            ];
            rows.sort_by(|a, b| row_aware_span_cmp(a.0, a.1, b.0, b.1));
            assert_eq!(rows[0].2, "top");
            assert_eq!(rows[1].2, "middle");
            assert_eq!(rows[2].2, "bottom");
        }

        /// The comparator is used by sort_by, which requires a valid total
        /// order. Run a randomized stress test to confirm no transitivity
        /// panics.
        #[test]
        fn test_row_aware_span_cmp_is_total_order() {
            let mut v: Vec<(f32, f32)> = (0..200)
                .map(|i| ((i as f32) * 0.73, ((i * 17) % 500) as f32))
                .collect();
            v.sort_by(|a, b| row_aware_span_cmp(a.0, a.1, b.0, b.1));
        }

        /// #656/#657: the RTL variant keeps rows top-to-bottom but orders
        /// X *descending* (right-to-left) within a row — a pure-RTL line's
        /// logical reading order.
        #[test]
        fn test_row_aware_span_cmp_rtl_within_row_is_descending() {
            // Same row (Y within band), laid out left-to-right by X.
            let mut row = [
                (100.0f32, 10.0f32, "leftmost"),
                (100.0, 50.0, "mid"),
                (100.0, 90.0, "rightmost"),
            ];
            row.sort_by(|a, b| row_aware_span_cmp_rtl(a.0, a.1, b.0, b.1));
            // Rightmost (highest X) reads first in RTL.
            assert_eq!(
                ["rightmost", "mid", "leftmost"],
                [row[0].2, row[1].2, row[2].2]
            );
        }

        /// Rows still order top-to-bottom regardless of the within-row flip.
        #[test]
        fn test_row_aware_span_cmp_rtl_rows_top_to_bottom() {
            let mut rows = [
                (10.0f32, 0.0f32, "bottom"),
                (100.0, 0.0, "top"),
                (50.0, 0.0, "middle"),
            ];
            rows.sort_by(|a, b| row_aware_span_cmp_rtl(a.0, a.1, b.0, b.1));
            assert_eq!(
                ["top", "middle", "bottom"],
                [rows[0].2, rows[1].2, rows[2].2]
            );
        }

        /// Must be a valid total order for `sort_by` (no transitivity panic).
        #[test]
        fn test_row_aware_span_cmp_rtl_is_total_order() {
            let mut v: Vec<(f32, f32)> = (0..200)
                .map(|i| ((i as f32) * 0.73, ((i * 17) % 500) as f32))
                .collect();
            v.sort_by(|a, b| row_aware_span_cmp_rtl(a.0, a.1, b.0, b.1));
        }

        /// Sort a large array with mixed NaN/normal values to stress-test.
        #[test]
        fn test_sort_stress_with_nan() {
            let mut values: Vec<f32> = (0..100).map(|i| i as f32).collect();
            // Insert NaN at various positions
            for i in (0..100).step_by(7) {
                values[i] = f32::NAN;
            }
            // Must not panic
            values.sort_by(|a, b| safe_float_cmp(*a, *b));
        }

        #[test]
        fn test_safe_prefix_ascii() {
            assert_eq!(safe_prefix("hello", 3), "hel");
            assert_eq!(safe_prefix("hello", 10), "hello");
            assert_eq!(safe_prefix("", 5), "");
            assert_eq!(safe_prefix("hi", 0), "");
        }

        #[test]
        fn test_safe_prefix_multibyte() {
            let text = "✚✳★✵"; // 4 × 3-byte chars = 12 bytes
            assert_eq!(safe_prefix(text, 10), "✚✳★"); // rounds down from 10 to 9
            assert_eq!(safe_prefix(text, 9), "✚✳★"); // exact boundary
            assert_eq!(safe_prefix(text, 12), "✚✳★✵"); // full string
        }

        #[test]
        fn test_safe_suffix_ascii() {
            assert_eq!(safe_suffix("hello", 3), "llo");
            assert_eq!(safe_suffix("hello", 10), "hello");
            assert_eq!(safe_suffix("", 5), "");
            assert_eq!(safe_suffix("hi", 0), "");
        }

        #[test]
        fn test_safe_suffix_multibyte() {
            let text = "AB✚✳★✵"; // 14 bytes: A(0) B(1) ✚(2..5) ✳(5..8) ★(8..11) ✵(11..14)
                                 // 14 - 10 = 4, byte 4 is inside ✚ → rounds up to 5
            assert_eq!(safe_suffix(text, 10), "✳★✵");
        }
    }
}

// Version info
/// Library version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Library name
pub const NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        // VERSION is populated from CARGO_PKG_VERSION at compile time
        assert!(VERSION.starts_with("0."));
    }

    #[test]
    fn test_name() {
        assert_eq!(NAME, "pdf_oxide");
    }
}
