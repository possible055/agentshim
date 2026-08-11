//! Spatial table detection from PDF text layout.
//!
//! Implements table detection according to ISO 32000-1:2008 Section 5.2 (Coordinate Systems).
//! Uses X and Y coordinate clustering to identify table structure in PDFs that lack explicit
//! table markup in the structure tree.

use crate::layout::text_block::TextSpan;
use crate::structure::table_extractor::{span_text_for_cell, Table, TableCell, TableRow};
use std::collections::HashMap;

mod cells;
mod intersection_grid;
mod intersections;
mod line_clustering;
mod line_tables;
mod quality;
mod text_grid;

use cells::*;
use intersection_grid::*;
use intersections::*;
use line_clustering::*;
use line_tables::*;
use quality::*;
use text_grid::*;

/// Disjoint-set (union-find) with path compression.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, i: usize) -> usize {
        let mut curr = i;
        while self.parent[curr] != curr {
            self.parent[curr] = self.parent[self.parent[curr]];
            curr = self.parent[curr];
        }
        curr
    }

    fn union(&mut self, i: usize, j: usize) {
        let ri = self.find(i);
        let rj = self.find(j);
        if ri != rj {
            self.parent[ri] = rj;
        }
    }

    fn groups(&mut self) -> HashMap<usize, Vec<usize>> {
        let mut result: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..self.parent.len() {
            let root = self.find(i);
            result.entry(root).or_default().push(i);
        }
        result
    }
}

/// Strategy for detecting table boundaries (v0.3.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum TableStrategy {
    /// Use only vector lines to define boundaries.
    #[serde(rename = "lines")]
    Lines,
    /// Use only text alignment to define boundaries.
    #[serde(rename = "text")]
    Text,
    /// Use both text and lines (hybrid approach).
    #[default]
    #[serde(rename = "both")]
    Both,
}

/// Configuration for spatial table detection.
#[derive(Debug, Clone, PartialEq)]
pub struct TableDetectionConfig {
    /// Whether table detection is enabled.
    pub enabled: bool,
    /// Strategy for horizontal boundary detection.
    pub horizontal_strategy: TableStrategy,
    /// Strategy for vertical boundary detection.
    pub vertical_strategy: TableStrategy,
    /// X-coordinate tolerance for column grouping.
    pub column_tolerance: f32,
    /// Y-coordinate tolerance for row grouping.
    pub row_tolerance: f32,
    /// Minimum number of cells required for a valid table.
    pub min_table_cells: usize,
    /// Minimum number of columns required for a valid table.
    pub min_table_columns: usize,
    /// Ratio of regular rows required for a valid table structure.
    pub regular_row_ratio: f32,
    /// Maximum number of columns allowed before rejecting as false positive.
    pub max_table_columns: usize,
    /// Merge threshold for post-clustering column merge pass.
    /// Adjacent columns whose centers are within this distance are merged.
    pub column_merge_threshold: f32,
    /// Minimum gap between Y-range groups of vertical lines to trigger a cluster split.
    /// Default: 20.0. Use smaller values (e.g. 4.0) for strict mode, larger (e.g. 40.0)
    /// for relaxed mode where V-lines at mixed Y-ranges should stay together.
    pub v_split_gap: f32,
    /// Enable text-only spatial detection as a fallback when no ruling lines are found.
    ///
    /// When `true` and the page has no table-relevant paths (no ruling lines or
    /// rectangles), the detector falls through to `detect_tables_from_spans_column_aware`
    /// rather than returning an empty result.  This is the right default for structured
    /// output callers (`to_markdown`, `to_html`) that explicitly want tabular layout
    /// and is also relied on by the public `extract_tables` API for line-less PDFs.
    /// Set to `false` from callers that want the conservative
    /// "no ruling lines → no tables" behaviour (e.g. plain-text extraction paths
    /// that explicitly opt out — see `extract_page_tables`).
    ///
    /// False-positive prose / TOC / underline tables that this default would
    /// previously have surfaced are filtered post-detection by the
    /// `looks_like_prose_table` shape gate and a ≥ 3-row evidence requirement
    /// on text-only and h-rule paths.
    ///
    /// Default: `true`.
    pub text_fallback: bool,
}

impl Default for TableDetectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            horizontal_strategy: TableStrategy::Both,
            vertical_strategy: TableStrategy::Both,
            column_tolerance: 15.0,
            row_tolerance: 2.8,
            min_table_cells: 4,
            min_table_columns: 2,
            regular_row_ratio: 0.3,
            max_table_columns: 15,
            column_merge_threshold: 25.0,
            v_split_gap: 20.0,
            text_fallback: true,
        }
    }
}

impl TableDetectionConfig {
    /// Create a strict table detection configuration.
    pub fn strict() -> Self {
        Self {
            enabled: true,
            horizontal_strategy: TableStrategy::Lines,
            vertical_strategy: TableStrategy::Lines,
            column_tolerance: 2.0,
            row_tolerance: 1.0,
            min_table_cells: 6,
            min_table_columns: 3,
            regular_row_ratio: 0.8,
            max_table_columns: 12,
            column_merge_threshold: 10.0,
            v_split_gap: 4.0,
            text_fallback: true,
        }
    }

    /// Create a relaxed table detection configuration.
    pub fn relaxed() -> Self {
        Self {
            enabled: true,
            horizontal_strategy: TableStrategy::Text,
            vertical_strategy: TableStrategy::Text,
            column_tolerance: 10.0,
            row_tolerance: 5.0,
            min_table_cells: 4,
            min_table_columns: 2,
            regular_row_ratio: 0.3,
            max_table_columns: 20,
            column_merge_threshold: 30.0,
            v_split_gap: 40.0,
            text_fallback: true,
        }
    }
}

/// Column-aware text-only table detection.
///
/// Detects page columns first (via X-projection histogram), then runs
/// `detect_tables_from_spans()` independently on each column partition.
/// This prevents multi-column academic layouts from being misinterpreted
/// as wide tables spanning the whole page.
pub fn detect_tables_from_spans_column_aware(
    spans: &[TextSpan],
    config: &TableDetectionConfig,
) -> Vec<Table> {
    if !config.enabled || spans.is_empty() {
        return Vec::new();
    }

    let page_cols = detect_page_columns(spans);

    // Single column (or none) → delegate directly.
    if page_cols.len() <= 1 {
        return detect_tables_from_spans(spans, config);
    }

    // Multiple columns → partition spans and detect per column.
    let mut all_tables = Vec::new();
    for &(col_x_min, col_x_max) in &page_cols {
        let col_spans: Vec<TextSpan> = spans
            .iter()
            .filter(|s| {
                let span_center = s.bbox.x + s.bbox.width / 2.0;
                span_center >= col_x_min && span_center <= col_x_max
            })
            .cloned()
            .collect();
        if col_spans.is_empty() {
            continue;
        }
        let mut tables = detect_tables_from_spans(&col_spans, config);
        all_tables.append(&mut tables);
    }

    all_tables
}

/// Detect tables from spatial layout of text spans.
pub fn detect_tables_from_spans(spans: &[TextSpan], config: &TableDetectionConfig) -> Vec<Table> {
    if !config.enabled || spans.is_empty() {
        return Vec::new();
    }

    let mut columns = detect_columns(
        spans,
        config.column_tolerance,
        config.column_merge_threshold,
    );

    // Greedy X-center clustering fragments a single logical cell whose
    // words are internally spaced (e.g. an agenda row "Receiving Dock
    // Inspection" laid out with wide inter-word gaps) into one column
    // per word. detect_text_edge_columns instead keeps only X edges that
    // recur across >= 3 distinct rows, so single-row word positions are
    // rejected and the true column grid (Time / Activity / Team) is
    // recovered. Cross-row recurrence is a strictly stronger column
    // signal than one row's word spacing, so prefer the text-edge result
    // whenever it yields a valid, strictly-smaller column set.
    //
    // Safety: for tables with < 3 rows, text-edge can keep no column
    // (every edge appears in < 3 rows) so it returns fewer than
    // min_table_columns and the guard below leaves greedy untouched —
    // small genuine tables are unaffected.
    // If greedy clustering produced too many columns, try text-edge
    // detection which looks for X positions that recur across multiple rows.
    if columns.len() > config.max_table_columns {
        let te_columns = detect_text_edge_columns(spans, config);
        if te_columns.len() >= config.min_table_columns.max(2) && te_columns.len() < columns.len() {
            columns = te_columns;
        }
    }

    // Borderless numeric lattice (ML / results tables). When the column gap
    // is below `column_merge_threshold`, greedy clustering fuses a dense grid
    // of short numeric cells laid out on a regular ~20pt pitch, so two values
    // share one cell ("0.69 0.76"). The text-edge detector keeps only X edges
    // that recur across >=3 rows, which on a numeric lattice recovers every
    // column. Prefer it when the spans are predominantly numeric and it splits
    // a coarser greedy set into more (still bounded) columns. The numeric-
    // predominance gate keeps prose / label-value tables (e.g. Google-Docs
    // exports) on the greedy path untouched.
    let numeric_spans = spans
        .iter()
        .filter(|s| is_numeric_cell(s.text.trim()))
        .count();
    if numeric_spans >= 10 && columns.len() <= config.max_table_columns {
        let te_columns = detect_text_edge_columns(spans, config);
        if te_columns.len() > columns.len()
            && te_columns.len() >= 5
            && te_columns.len() <= config.max_table_columns
            && is_regular_lattice(&te_columns)
        {
            // Adopt the finer lattice ONLY when it still forms a fully valid
            // grid. A sparse split that fails the quality gate would otherwise
            // drop the whole table to prose — worse than the merged-column
            // baseline. Probing here means the refinement can only refine a
            // table that stays valid, never demote one.
            let probe_rows = detect_rows(spans, config.row_tolerance);
            if probe_rows.len() >= 2 {
                let probe_grid = assign_spans_to_cells(spans, &te_columns, &probe_rows);
                if validate_table_structure_internal(&probe_grid, config) {
                    let probe_table = grid_to_table(&probe_grid, spans, None);
                    if is_valid_table(&probe_table)
                        && passes_spatial_quality_gate(&probe_table)
                        && !looks_like_prose_paragraph(&probe_table)
                    {
                        columns = te_columns;
                    }
                }
            }
        }
    }

    if columns.len() < config.min_table_columns.max(2) || columns.len() > config.max_table_columns {
        return Vec::new();
    }

    let rows = detect_rows(spans, config.row_tolerance);
    if rows.len() < 2 {
        return Vec::new();
    }

    // Baseline gate (CRITICAL): the ORIGINAL (unfiltered) columns must
    // already form a table that passes EVERY emission gate baseline
    // uses — structural validation AND the final is_valid_table /
    // passes_spatial_quality_gate checks. The row-coverage cleanup
    // below only REFINES a table that would have been emitted anyway;
    // it must never CREATE a table from content baseline treated as
    // prose. Without checking the FINAL gates here, dropping phantom
    // columns can flip a borderline case that baseline rejected on the
    // quality gate into a spurious table (observed on annots.pdf link
    // lists and right_to_left_01.pdf Arabic prose in the 70-PDF sweep).
    let orig_grid = assign_spans_to_cells(spans, &columns, &rows);
    if !validate_table_structure_internal(&orig_grid, config) {
        return Vec::new();
    }
    let orig_table = grid_to_table(&orig_grid, spans, None);
    if !is_valid_table(&orig_table)
        || !passes_spatial_quality_gate(&orig_table)
        || looks_like_prose_paragraph(&orig_table)
        || looks_like_bulleted_list(&orig_table)
        || looks_like_cjk_prose(&orig_table)
    {
        return Vec::new();
    }

    // Issue #6/#5: drop "phantom" columns created by a single cell whose
    // words are spaced apart (e.g. an agenda "Receiving Dock Inspection"
    // laid out with wide gaps → one greedy column per word). A genuine
    // table column carries content in MOST rows; a per-word phantom
    // appears in only one or two. Keep only columns whose spans occupy
    // at least 60% of rows (min 2). Phantom-column spans are then
    // re-assigned to the nearest surviving column by assign_spans_to_cells,
    // re-joining the words into their true cell. Skipped for small
    // tables (< 3 rows) where every column legitimately spans all rows.
    if rows.len() >= 3 {
        columns = filter_columns_by_row_coverage(&columns, &rows, spans);
        if columns.len() < config.min_table_columns.max(2) {
            return Vec::new();
        }
    }

    let grid = assign_spans_to_cells(spans, &columns, &rows);
    if !validate_table_structure_internal(&grid, config) {
        return Vec::new();
    }

    let table = grid_to_table(&grid, spans, None);
    if !is_valid_table(&table)
        || !passes_spatial_quality_gate(&table)
        || looks_like_prose_paragraph(&table)
        || looks_like_bulleted_list(&table)
        || looks_like_cjk_prose(&table)
    {
        return Vec::new();
    }
    vec![table]
}

#[derive(Debug, Clone)]
struct ColumnCluster {
    x_center: f32,
    x_min: f32,
    x_max: f32,
    span_indices: Vec<usize>,
}

#[derive(Debug, Clone)]
struct RowCluster {
    y_center: f32,
    y_min: f32,
    y_max: f32,
    span_indices: Vec<usize>,
}

#[derive(Debug, Clone)]
struct GridStructure {
    columns: Vec<ColumnCluster>,
    rows: Vec<RowCluster>,
    cells: Vec<Vec<Vec<usize>>>,
}

impl GridStructure {
    fn is_row_empty(&self, row_idx: usize) -> bool {
        self.cells[row_idx].iter().all(|cell| cell.is_empty())
    }

    fn is_column_empty(&self, col_idx: usize) -> bool {
        for row in &self.cells {
            if !row[col_idx].is_empty() {
                return false;
            }
        }
        true
    }

    fn trim_empty_columns(&self) -> GridStructure {
        let num_rows = self.cells.len();
        let num_cols = self.columns.len();

        let mut first_col = 0;
        while first_col < num_cols && self.is_column_empty(first_col) {
            first_col += 1;
        }

        let mut last_col = num_cols;
        while last_col > first_col && self.is_column_empty(last_col - 1) {
            last_col -= 1;
        }

        if first_col >= last_col {
            return self.clone();
        }

        let mut active_cols = Vec::new();
        for c in first_col..last_col {
            let col_width = self.columns[c].x_max - self.columns[c].x_min;
            if col_width < 2.0 && self.is_column_empty(c) {
                continue;
            }
            active_cols.push(c);
        }

        if active_cols.is_empty() {
            return self.clone();
        }

        let new_columns: Vec<ColumnCluster> = active_cols
            .iter()
            .map(|&c| self.columns[c].clone())
            .collect();

        let mut new_cells = Vec::with_capacity(num_rows);
        for r in 0..num_rows {
            let row_cells = active_cols
                .iter()
                .map(|&c| self.cells[r][c].clone())
                .collect();
            new_cells.push(row_cells);
        }

        GridStructure {
            columns: new_columns,
            rows: self.rows.clone(),
            cells: new_cells,
        }
    }
}

#[derive(Debug, Clone)]
struct CellMergeInfo {
    colspan: u32,
    rowspan: u32,
    covered: bool,
}

/// Backward compatibility: Indices of spans belonging to a table.
#[derive(Debug, Clone)]
pub struct DetectedTable {
    /// Indices of spans that belong to this table.
    pub span_indices: Vec<usize>,
}

/// Backward compatibility: Table detector wrapper.
pub struct SpatialTableDetector {
    /// Configuration for this detector.
    pub config: TableDetectionConfig,
}

impl SpatialTableDetector {
    /// Create a new detector with config.
    pub fn with_config(config: TableDetectionConfig) -> Self {
        Self { config }
    }
    /// Detect tables (wrapper).
    pub fn detect_tables(&self, spans: &[TextSpan]) -> Vec<DetectedTable> {
        detect_tables_from_spans_column_aware(spans, &self.config)
            .into_iter()
            .flat_map(|_| None)
            .collect()
    }
    /// Detect tables using visual lines and text (hybrid).
    pub fn detect_tables_hybrid(
        &self,
        spans: &[TextSpan],
        lines: &[crate::elements::PathContent],
    ) -> Vec<Table> {
        detect_tables_with_lines(spans, lines, &self.config)
    }
}

struct LineCluster {
    lines: Vec<usize>,
    bbox: crate::geometry::Rect,
}

impl LineCluster {
    fn new(line_idx: usize, bbox: crate::geometry::Rect) -> Self {
        Self {
            lines: vec![line_idx],
            bbox,
        }
    }
    fn add(&mut self, line_idx: usize, bbox: crate::geometry::Rect) {
        self.lines.push(line_idx);
        self.bbox = self.bbox.union(&bbox);
    }
}

/// A horizontal or vertical edge (segment).
#[derive(Debug, Clone, Copy)]
struct Edge {
    /// For H edges: the shared y coordinate. For V edges: the shared x coordinate.
    coord: f32,
    /// Start of the range (min x for H, min y for V).
    start: f32,
    /// End of the range (max x for H, max y for V).
    end: f32,
}

/// An intersection point on the grid.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Intersection {
    x: f32,
    y: f32,
}

/// A rectangular cell defined by four corner intersections.
#[derive(Debug, Clone, Copy)]
struct IntersectionCell {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

/// Detect tables using vector lines and text spans (main entry point for hybrid detection).
pub fn detect_tables_with_lines(
    spans: &[TextSpan],
    lines: &[crate::elements::PathContent],
    config: &TableDetectionConfig,
) -> Vec<Table> {
    if !config.enabled || spans.is_empty() {
        return Vec::new();
    }
    match (config.horizontal_strategy, config.vertical_strategy) {
        (TableStrategy::Text, TableStrategy::Text) => {
            return detect_tables_from_spans_column_aware(spans, config)
        }
        (TableStrategy::Lines, TableStrategy::Lines) => {
            // Try intersection-based detection first; fall back to cluster-based.
            let tables = detect_tables_from_intersections(spans, lines, config);
            if !tables.is_empty() {
                return tables.into_iter().filter(is_valid_table).collect();
            }
            let clusters = group_lines_into_clusters(lines, config);
            let mut tables = Vec::new();
            for cluster in clusters {
                tables.append(&mut detect_tables_in_cluster(
                    spans, lines, &cluster, config,
                ));
            }
            return tables.into_iter().filter(is_valid_table).collect();
        }
        _ => {}
    }
    // Both / hybrid strategy: try intersection-based first, then cluster, then H-rule bounded,
    // then text fallback.
    let mut final_tables = detect_tables_from_intersections(spans, lines, config);
    if final_tables.is_empty() {
        let clusters = group_lines_into_clusters(lines, config);
        for cluster in clusters {
            final_tables.append(&mut detect_tables_in_cluster(
                spans, lines, &cluster, config,
            ));
        }
    }
    // When intersection and cluster pipelines found nothing, try H-rule bounded detection:
    // use horizontal lines as table region boundaries with text-edge column detection.
    if final_tables.is_empty() {
        let (mut h_edges, _) = extract_edges(lines);
        if !h_edges.is_empty() && !has_vertical_ruling_evidence(lines, &h_edges) {
            snap_and_merge(&mut h_edges);
            final_tables = detect_tables_from_horizontal_rules(spans, &h_edges, config);
            // A logical table ruled between row *bands* (a rule under the
            // header, or between groups of rows) is emitted here as one
            // fragment per band. Merge vertically-adjacent same-column
            // fragments BEFORE the min-row filter so a table cut into e.g. a
            // [3, 2] pair rejoins into [5] instead of losing its short band to
            // the guard below. Bands sit ~one inter-row pitch apart (the rule
            // stroke + leading), so scale the vertical tolerance to the
            // fragments' median row height rather than the abutting-fragment
            // default. Safety rests on the unchanged column gating in
            // `can_merge_tables` (equal col_count + matched X-start/width): a
            // lone spurious 2-row prose strip has no same-column neighbour, so
            // it stays short and is still dropped — the guard's intent holds.
            let row_h = median_fragment_row_height(&final_tables);
            let y_tol = (row_h * 1.5).max(3.0);
            final_tables = consolidate_adjacent_table_fragments_with_tol(final_tables, 2.0, y_tol);
            // H-rule bounded detection lacks vertical-line evidence —
            // columns come from text-edge clustering alone (same shape as
            // the text-only fallback below).  Two-row results are
            // virtually always prose that happens to live between
            // decorative rules (annotation underlines, page borders);
            // require three rows of evidence before promoting.
            final_tables.retain(|t| t.rows.len() >= 3);
        }
    }
    // Filter out invalid line-based tables BEFORE overlap checking so that
    // spurious line-based tables don't shadow valid text-based ones.
    final_tables.retain(is_valid_table);

    // Only allow text-based fallback if BOTH strategies permit it AND the caller
    // explicitly enabled text-only detection (config.text_fallback=true).
    // This prevents extract_text() callers (text_fallback=false) from
    // spuriously running span-column detection alongside ruling-line tables:
    // report-style PDFs with decorative horizontal rules (e.g. swimming results)
    // would otherwise have all their data detected as a text table that renders
    // the page content a second time, causing duplicate extraction.
    // Callers that want text-based table detection (to_markdown, to_html) set
    // config.text_fallback=true explicitly.
    let allow_text_fallback = config.text_fallback
        && config.horizontal_strategy != TableStrategy::Lines
        && config.vertical_strategy != TableStrategy::Lines;

    if allow_text_fallback {
        let text_candidates = detect_tables_from_spans_column_aware(spans, config);
        for text_table in text_candidates {
            if !passes_spatial_quality_gate(&text_table) {
                continue;
            }
            // Text-only detection (no ruling lines) infers columns from word
            // x-alignment alone — two rows of column-aligned words is the
            // signature of ordinary prose (a title + a wrapped body line),
            // not a table.  Require at least three rows of evidence before
            // promoting a span cluster to a table.
            if text_table.rows.len() < 3 {
                continue;
            }
            if let Some(text_bbox) = text_table.bbox {
                let overlaps = final_tables.iter().any(|t| {
                    if let Some(line_bbox) = t.bbox {
                        line_bbox.intersects(&text_bbox)
                            || line_bbox.contains_rect(&text_bbox)
                            || text_bbox.contains_rect(&line_bbox)
                    } else {
                        false
                    }
                });
                if !overlaps {
                    final_tables.push(text_table);
                }
            }
        }
    }
    final_tables
}

/// Consolidate vertically-adjacent tables that share an identical column
/// structure into a single multi-row table.
///
/// Issue 484/486/487 root cause: when a logical multi-row table is drawn
/// with a horizontal ruling line between every pair of rows (rather than
/// only at the top and bottom), the line-based detector emits one Table
/// per row strip. Each fragment is a 1- or 2-row table that fails
/// `is_real_grid()` (which requires ≥2 rows) and gets dropped, after
/// which the cells fall through to the paragraph flow with column-based
/// reading order — producing orphan `<p>40000≤Q</p>` / `<p>＜55000</p>`
/// pairs instead of `<table><td>40000≤Q＜55000</td></tr></table>`.
///
/// Two fragments are merge-candidates when:
///   * both have a `bbox`
///   * X start matches within `X_TOLERANCE`
///   * width matches within `X_TOLERANCE`
///   * column counts are equal
///   * the lower fragment's top edge (`bbox.y + bbox.height`) is within
///     `Y_TOLERANCE` of the upper fragment's bottom edge (`bbox.y`)
///
/// Sort tables top-down (PDF y-up: largest top-Y first) and merge runs
/// of consecutive fragments that satisfy the criteria. The merged table
/// preserves the union of all rows and a bbox spanning both fragments.
pub fn consolidate_adjacent_table_fragments(tables: Vec<Table>) -> Vec<Table> {
    consolidate_adjacent_table_fragments_with_tol(tables, 2.0, 3.0)
}

/// Like [`consolidate_adjacent_table_fragments`] but with a caller-chosen
/// vertical merge tolerance. The default 3.0 pt tolerance assumes fragments
/// abut (a ruling line between every row leaves ~0 gap). The H-rule-bounded
/// detector, however, emits one fragment per rule-delimited band, so two
/// bands of the SAME logical table are separated by a full inter-row pitch
/// (rule stroke + leading) — often ~10-15 pt. A larger `y_tol` lets those
/// bands rejoin. Safety rests on the unchanged column gating in
/// `can_merge_tables` (equal col_count + X-start ≤ x_tol + width ≤ x_tol):
/// two genuinely distinct tables sharing all three within one row-height are
/// vanishingly rare, whereas bands of one ruled table match them exactly.
pub fn consolidate_adjacent_table_fragments_with_tol(
    tables: Vec<Table>,
    x_tolerance: f32,
    y_tolerance: f32,
) -> Vec<Table> {
    if tables.len() < 2 {
        return tables;
    }

    // Sort by top-Y descending (top of page first in PDF y-up coordinates).
    let mut sorted = tables;
    sorted.sort_by(|a, b| {
        let a_top = a.bbox.map(|b| b.y + b.height).unwrap_or(f32::NEG_INFINITY);
        let b_top = b.bbox.map(|b| b.y + b.height).unwrap_or(f32::NEG_INFINITY);
        crate::utils::safe_float_cmp(b_top, a_top)
    });

    let mut consolidated: Vec<Table> = Vec::with_capacity(sorted.len());
    for table in sorted {
        let merge_into_last = consolidated
            .last()
            .map(|last| can_merge_tables(last, &table, x_tolerance, y_tolerance))
            .unwrap_or(false);
        if merge_into_last {
            // Safety: merge_into_last is only true when consolidated.last()
            // returned Some, so last_mut() must also return Some.
            if let Some(last) = consolidated.last_mut() {
                merge_table_into(last, table);
            }
        } else {
            consolidated.push(table);
        }
    }
    consolidated
}

#[cfg(test)]
mod tests;
