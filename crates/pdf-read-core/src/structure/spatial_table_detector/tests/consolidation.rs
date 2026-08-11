use super::*;

/// Two vertically-adjacent fragments with identical column structure
/// merge into a single multi-row table.  Models the issue-336 / nougat_018
/// fragmented-table pattern: every horizontal ruling line produces a
/// separate 1-row Table.
#[test]
fn consolidate_merges_adjacent_aligned_fragments() {
    let upper = make_fragment(90.0, 480.0, 420.0, 16.0, 8);
    let lower = make_fragment(90.0, 464.0, 420.0, 16.0, 8); // upper.bottom = 480, lower.top = 480
    let merged = consolidate_adjacent_table_fragments(vec![upper, lower]);
    assert_eq!(merged.len(), 1, "two aligned adjacent fragments must merge");
    assert_eq!(merged[0].rows.len(), 2, "merged rows are concatenated");
    let bb = merged[0].bbox.expect("merged bbox preserved");
    assert!((bb.x - 90.0).abs() < 0.1);
    assert!((bb.width - 420.0).abs() < 0.1);
    // Total height covers both fragments.
    assert!((bb.height - 32.0).abs() < 0.1);
}

/// Fragments separated by more than `Y_TOLERANCE = 3pt` must NOT merge —
/// they represent two distinct tables (e.g. two unrelated grids on the
/// same page).
#[test]
fn consolidate_does_not_merge_non_adjacent_fragments() {
    let upper = make_fragment(90.0, 600.0, 420.0, 16.0, 8);
    let lower = make_fragment(90.0, 400.0, 420.0, 16.0, 8); // 200pt gap
    let result = consolidate_adjacent_table_fragments(vec![upper, lower]);
    assert_eq!(result.len(), 2, "well-separated fragments stay distinct");
}

/// A table ruled between row *bands* (not every row) emits fragments
/// separated by a full inter-row pitch, not abutting. The default 3pt
/// tolerance leaves them split (dropping the short band to the >=3-row
/// guard); the row-height-scaled tolerance rejoins them. Models the
/// PMC8103272 Table-1 [3, 2] → [5] recovery.
#[test]
fn consolidate_with_tol_merges_row_pitch_separated_bands() {
    // Fragment with `extra` additional empty rows beyond make_fragment's one.
    let frag = |x, y, cols, rows: usize| {
        let mut t = make_fragment(x, y, 420.0, 16.0, cols);
        for _ in 1..rows {
            t.rows.push(TableRow::new(false));
        }
        t
    };
    // upper.bottom = 480; lower.top = 452 + 16 = 468 → 12pt gap (one pitch).
    let upper = frag(90.0, 480.0, 6, 3);
    let lower = frag(90.0, 452.0, 6, 2);
    // Default tolerance: bands stay split (this is the pre-fix behaviour).
    let split = consolidate_adjacent_table_fragments(vec![upper.clone(), lower.clone()]);
    assert_eq!(split.len(), 2, "3pt tolerance leaves row-pitch bands split");
    // Row-height-scaled tolerance (1.5 * ~16pt row height = 24 > 12pt gap): merge.
    let merged = consolidate_adjacent_table_fragments_with_tol(vec![upper, lower], 2.0, 24.0);
    assert_eq!(
        merged.len(),
        1,
        "row-pitch bands rejoin under scaled tolerance"
    );
    assert_eq!(
        merged[0].rows.len(),
        5,
        "[3,2] bands rejoin into a 5-row table"
    );
}

/// The scaled tolerance still refuses fragments that are farther apart
/// than a plausible single row pitch — two genuinely distinct grids.
#[test]
fn consolidate_with_tol_still_rejects_far_bands() {
    let upper = make_fragment(90.0, 600.0, 420.0, 16.0, 6);
    let lower = make_fragment(90.0, 400.0, 420.0, 16.0, 6); // 184pt gap
    let merged = consolidate_adjacent_table_fragments_with_tol(vec![upper, lower], 2.0, 24.0);
    assert_eq!(
        merged.len(),
        2,
        "a scaled tolerance is not an unbounded merge"
    );
}

/// Fragments with different column counts must NOT merge even if
/// they sit adjacent vertically — they aren't a single logical table.
#[test]
fn consolidate_does_not_merge_different_col_counts() {
    let upper = make_fragment(90.0, 480.0, 420.0, 16.0, 8);
    let lower = make_fragment(90.0, 464.0, 420.0, 16.0, 5); // different col_count
    let result = consolidate_adjacent_table_fragments(vec![upper, lower]);
    assert_eq!(result.len(), 2, "different col_count blocks merging");
}

/// Fragments at different x-positions must NOT merge — they're in
/// different columns of the page even if their Y ranges are adjacent.
#[test]
fn consolidate_does_not_merge_misaligned_x() {
    let upper = make_fragment(90.0, 480.0, 420.0, 16.0, 8);
    let lower = make_fragment(50.0, 464.0, 420.0, 16.0, 8); // x offset 40pt
    let result = consolidate_adjacent_table_fragments(vec![upper, lower]);
    assert_eq!(result.len(), 2, "misaligned-x blocks merging");
}

/// A chain of three+ adjacent fragments should chain-merge into one
/// multi-row table.  This is the issue-336 shape: 8 column-aligned
/// fragments → one 18-row table after consolidation.
#[test]
fn consolidate_chains_multiple_adjacent_fragments() {
    let f1 = make_fragment(90.0, 480.0, 420.0, 16.0, 8);
    let f2 = make_fragment(90.0, 464.0, 420.0, 16.0, 8);
    let f3 = make_fragment(90.0, 448.0, 420.0, 16.0, 8);
    let f4 = make_fragment(90.0, 432.0, 420.0, 16.0, 8);
    let merged = consolidate_adjacent_table_fragments(vec![f1, f2, f3, f4]);
    assert_eq!(merged.len(), 1, "chain of 4 adjacent fragments → 1 table");
    assert_eq!(merged[0].rows.len(), 4, "all rows preserved in chain merge");
}

/// Small vertical overlap (line-detector quirk) up to half the
/// smaller fragment's height is still considered adjacent.
#[test]
fn consolidate_tolerates_small_overlap() {
    // upper bottom = 480, lower top = 484 (4pt overlap, smaller_h = 16, half = 8)
    let upper = make_fragment(90.0, 480.0, 420.0, 16.0, 8);
    let lower = make_fragment(90.0, 468.0, 420.0, 16.0, 8);
    let merged = consolidate_adjacent_table_fragments(vec![upper, lower]);
    assert_eq!(merged.len(), 1, "small overlap (< ½ row) still merges");
}

/// Empty input passes through unchanged.
#[test]
fn consolidate_empty_input() {
    let merged = consolidate_adjacent_table_fragments(vec![]);
    assert!(merged.is_empty());
}

/// Single-table input passes through unchanged (nothing to merge).
#[test]
fn consolidate_single_table_passthrough() {
    let t = make_fragment(90.0, 480.0, 420.0, 16.0, 8);
    let merged = consolidate_adjacent_table_fragments(vec![t]);
    assert_eq!(merged.len(), 1);
}

/// Adjacent spans with a sub-em gap must NOT have a separator
/// inserted.  Models issue-336's `60000` + `≤` + `Q` + `＜` + `80000`
/// where the operator glyphs touch / slightly overlap the digit
/// glyphs and must be rendered as a single compound token.
#[test]
fn cell_span_separator_no_space_for_tight_glyphs() {
    let prev = ts("60000≤Q", 100.0, 200.0, 39.8, 10.6);
    let curr = ts("＜", 139.8, 200.0, 10.6, 10.6); // gap = 0
    assert_eq!(cell_span_separator(&prev, &curr), "");
}

/// Adjacent spans with a real gap (clear word boundary) get a
/// single space separator inserted.
#[test]
fn cell_span_separator_inserts_space_for_real_gap() {
    let prev = ts("Quarter", 100.0, 200.0, 30.0, 10.0); // ends at x=130
    let curr = ts("Total", 140.0, 200.0, 25.0, 10.0); // gap = 10pt = 1em
    assert_eq!(cell_span_separator(&prev, &curr), " ");
}

/// CJK ↔ fullwidth-operator boundary suppresses separator even when
/// the geometric gap would otherwise warrant a space.
#[test]
fn cell_span_separator_suppresses_cjk_fullwidth_boundary() {
    // "中" (U+4E2D, CJK) followed by "＜" (U+FF1C, fullwidth less-than).
    let prev = ts("中", 100.0, 200.0, 12.0, 12.0); // ends at 112
    let curr = ts("＜", 115.0, 200.0, 12.0, 12.0); // gap = 3pt = 0.25 em
    assert_eq!(cell_span_separator(&prev, &curr), "");
}

/// Trailing whitespace on the preceding span suppresses separator
/// (no double-space).
#[test]
fn cell_span_separator_suppresses_on_existing_trailing_space() {
    let prev = ts("Quarter ", 100.0, 200.0, 35.0, 10.0); // text ends with ' '
    let curr = ts("Total", 140.0, 200.0, 25.0, 10.0);
    assert_eq!(cell_span_separator(&prev, &curr), "");
}
