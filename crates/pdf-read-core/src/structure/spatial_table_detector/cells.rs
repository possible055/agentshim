use super::*;

pub(super) fn grid_to_table(
    grid: &GridStructure,
    spans: &[TextSpan],
    visual_merge_info: Option<Vec<Vec<CellMergeInfo>>>,
) -> Table {
    let num_rows = grid.cells.len();
    let num_cols = grid.columns.len();
    let merge_info = visual_merge_info.unwrap_or_else(|| detect_merged_cells(grid, spans));
    let header_row_idx = detect_header_row(grid, spans);
    let mut table_rows = Vec::new();
    for (row_idx, row) in grid.cells.iter().enumerate() {
        let is_header = header_row_idx == Some(row_idx);
        let mut table_row = TableRow::new(is_header);
        for (col_idx, cell_span_indices) in row.iter().enumerate() {
            let mi = &merge_info[row_idx][col_idx];
            if mi.covered {
                continue;
            }
            let cell_text = extract_cell_text(cell_span_indices, spans);
            let mut cell_bbox = None;
            if !cell_span_indices.is_empty() {
                let mut b = spans[cell_span_indices[0]].bbox;
                for &idx in &cell_span_indices[1..] {
                    b = b.union(&spans[idx].bbox);
                }
                cell_bbox = Some(b);
            }
            let mcids = cell_span_indices
                .iter()
                .filter_map(|&idx| spans.get(idx).and_then(|s| s.mcid))
                .collect::<Vec<_>>();
            let cell_spans = cell_span_indices
                .iter()
                .filter_map(|&idx| spans.get(idx).cloned())
                .collect::<Vec<_>>();

            table_row.cells.push(TableCell {
                text: cell_text,
                spans: cell_spans,
                colspan: mi.colspan.min((num_cols - col_idx) as u32),
                rowspan: mi.rowspan.min((num_rows - row_idx) as u32),
                mcids,
                bbox: cell_bbox,
                is_header,
            });
        }
        table_rows.push(table_row);
    }
    let all_span_indices: Vec<usize> = grid
        .cells
        .iter()
        .flat_map(|row| row.iter().flat_map(|cell| cell.iter().copied()))
        .collect();
    let mut bbox = None;
    if !all_span_indices.is_empty() {
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for &idx in &all_span_indices {
            if let Some(s) = spans.get(idx) {
                min_x = min_x.min(s.bbox.x);
                min_y = min_y.min(s.bbox.y);
                max_x = max_x.max(s.bbox.x + s.bbox.width);
                max_y = max_y.max(s.bbox.y + s.bbox.height);
            }
        }
        bbox = Some(crate::geometry::Rect::new(
            min_x,
            min_y,
            max_x - min_x,
            max_y - min_y,
        ));
    }
    Table {
        rows: table_rows,
        has_header: header_row_idx.is_some(),
        col_count: num_cols,
        bbox,
    }
}

pub(super) fn extract_cell_text(cell_span_indices: &[usize], spans: &[TextSpan]) -> String {
    if cell_span_indices.is_empty() {
        return String::new();
    }
    // Keep the span reference (not just text) so we can decide spacing based
    // on the geometric gap and CJK/fullwidth-operator boundary state, exactly
    // like the inline-flow path does in pipeline/converters/mod.rs.  Without
    // this, the previous `line.join(" ")` was unconditionally inserting a
    // space between every adjacent span on the same row, splitting compound
    // tokens like `40000≤Q＜55000` into `40000≤Q ＜55000` and dropping word-F1
    // for table-heavy CJK documents (issue 484, issue-336).
    let mut span_entries: Vec<(f32, &TextSpan, String)> = cell_span_indices
        .iter()
        .filter_map(|&idx| {
            spans
                .get(idx)
                .map(|s| (s.bbox.center().y, s, span_text_for_cell(s)))
        })
        .collect();
    if span_entries.is_empty() {
        return String::new();
    }
    if span_entries.len() == 1 {
        return span_entries.remove(0).2;
    }
    span_entries.sort_by(|a, b| crate::utils::safe_float_cmp(b.0, a.0));

    // Group into rows by y proximity, then within a row decide separator per
    // pair of spans using the same gap/CJK rules as inline text assembly.
    let mut lines: Vec<Vec<(&TextSpan, String)>> = Vec::new();
    let mut current_line: Vec<(&TextSpan, String)> =
        vec![(span_entries[0].1, span_entries[0].2.clone())];
    let mut current_y = span_entries[0].0;
    for (y, span, text) in &span_entries[1..] {
        if (current_y - y).abs() <= 2.0 {
            current_line.push((span, text.clone()));
        } else {
            lines.push(current_line);
            current_line = vec![(span, text.clone())];
            current_y = *y;
        }
    }
    lines.push(current_line);

    lines
        .iter()
        .map(|line| {
            let mut out = String::new();
            for (i, (span, text)) in line.iter().enumerate() {
                if i > 0 {
                    let (prev_span, _) = line[i - 1];
                    let separator = cell_span_separator(prev_span, span);
                    out.push_str(separator);
                }
                out.push_str(text);
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decide what (if any) separator to put between two spans within the same
/// table cell row.  Mirrors the inline-flow has_horizontal_gap logic from
/// pipeline/converters/mod.rs: insert a space only when there is a real
/// horizontal gap that exceeds the inter-glyph kerning floor AND the
/// boundary is not a CJK ↔ CJK / CJK ↔ fullwidth-operator pair (issue 485).
pub(super) fn cell_span_separator(prev: &TextSpan, current: &TextSpan) -> &'static str {
    // Already-present whitespace at the join point — never duplicate.
    if prev.text.ends_with(' ') || current.text.starts_with(' ') {
        return "";
    }

    let prev_end_x = prev.bbox.x + prev.bbox.width;
    let gap = current.bbox.x - prev_end_x;
    let font_size = prev.font_size.max(current.font_size).max(1.0);

    // Sub-em gap: glyphs are touching or overlapping (typical inter-glyph
    // advance).  Don't insert a space — adjacent characters in the same
    // word/expression must stay glued.  This is what `40000≤Q` + `＜55000`
    // hits: gap is essentially zero (the source PDF emits the operator as
    // its own positioned Tj) but the two spans are part of one compound
    // token in pdftotext's output.
    let space_threshold = font_size * 0.15;
    if gap <= space_threshold {
        return "";
    }
    // No upper bound: a very large gap (≥ 5 em) used to be treated as a
    // column boundary and yield no separator, but the caller concatenates
    // span text when this returns "" — so tokens like `3.80%` and `4.41%`
    // were rendered as `3.80%4.41%` on wide rate tables.  Mirroring the
    // inline-flow rule (`pipeline::converters::has_horizontal_gap`), any
    // gap above the inter-glyph threshold now gets at least a single
    // space.

    // CJK / fullwidth-operator suppression — same rule as
    // pipeline::converters::has_horizontal_gap.  pdftotext keeps an
    // ideograph + adjacent fullwidth/math operator without a separator.
    let is_cjk = |c: char| {
        matches!(
            c as u32,
            0x3040..=0x309F     // Hiragana
            | 0x30A0..=0x30FF   // Katakana
            | 0x4E00..=0x9FFF   // CJK Unified Ideographs
            | 0xAC00..=0xD7AF   // Hangul
            | 0x3400..=0x4DBF   // CJK Extension A
            | 0x20000..=0x2A6DF // CJK Extension B
        )
    };
    let is_fw_op = |c: char| {
        matches!(
            c as u32,
            0xFF0B | 0xFF0D | 0xFF1A | 0xFF1B
            | 0xFF1C..=0xFF1E       // ＜ ＝ ＞
            | 0x2260 | 0x2248
            | 0x2264..=0x2265       // ≤ ≥
            | 0x00B5 | 0x03BC       // µ μ
            | 0x00B1 | 0x00D7 | 0x00F7
        )
    };
    let prev_tail = prev.text.chars().next_back();
    let curr_head = current.text.chars().next();
    if let (Some(p), Some(c)) = (prev_tail, curr_head) {
        let p_cjk = is_cjk(p);
        let c_cjk = is_cjk(c);
        if (p_cjk || is_fw_op(p)) && (c_cjk || is_fw_op(c)) && (p_cjk || c_cjk) {
            return "";
        }
    }

    " "
}

/// Median per-row height across table fragments, estimated as each
/// fragment's bbox height divided by its row count. Used to scale the
/// vertical merge tolerance for rule-split bands. Returns 0.0 when no
/// fragment carries usable geometry.
pub(super) fn median_fragment_row_height(tables: &[Table]) -> f32 {
    let mut heights: Vec<f32> = tables
        .iter()
        .filter_map(|t| {
            let b = t.bbox?;
            let n = t.rows.len();
            if n == 0 || b.height <= 0.0 {
                None
            } else {
                Some(b.height / n as f32)
            }
        })
        .collect();
    if heights.is_empty() {
        return 0.0;
    }
    heights.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
    heights[heights.len() / 2]
}

pub(super) fn can_merge_tables(upper: &Table, lower: &Table, x_tol: f32, y_tol: f32) -> bool {
    let (Some(u_bbox), Some(l_bbox)) = (upper.bbox, lower.bbox) else {
        return false;
    };
    if upper.col_count != lower.col_count || upper.col_count == 0 {
        return false;
    }
    if (u_bbox.x - l_bbox.x).abs() > x_tol {
        return false;
    }
    if (u_bbox.width - l_bbox.width).abs() > x_tol {
        return false;
    }
    // upper sits ABOVE lower in PDF y-up: upper.bbox.y is the BOTTOM of
    // upper, lower.bbox.y + lower.bbox.height is the TOP of lower.
    // For them to be vertically adjacent, the upper.bottom must be close
    // to the lower.top.  We allow a small NEGATIVE gap (overlap) up to
    // half the smaller table's height — the line-based detector
    // occasionally produces bboxes that overhang the adjacent table by a
    // few points when ruling-rule strokes have non-zero thickness or
    // include the line's drawn extent above/below the baseline.  Real
    // distinct tables almost always have a meaningful positive gap.
    let upper_bottom = u_bbox.y;
    let lower_top = l_bbox.y + l_bbox.height;
    let gap = upper_bottom - lower_top;
    if gap > y_tol {
        return false;
    }
    let min_height = u_bbox.height.min(l_bbox.height);
    if -gap > min_height * 0.5 {
        return false;
    }
    true
}

pub(super) fn merge_table_into(upper: &mut Table, lower: Table) {
    if let (Some(ub), Some(lb)) = (upper.bbox, lower.bbox) {
        let new_y = ub.y.min(lb.y);
        let new_top = (ub.y + ub.height).max(lb.y + lb.height);
        let new_x = ub.x.min(lb.x);
        let new_right = (ub.x + ub.width).max(lb.x + lb.width);
        upper.bbox = Some(crate::geometry::Rect {
            x: new_x,
            y: new_y,
            width: new_right - new_x,
            height: new_top - new_y,
        });
    }
    upper.rows.extend(lower.rows);
}

pub(super) fn detect_merged_cells(
    grid: &GridStructure,
    spans: &[TextSpan],
) -> Vec<Vec<CellMergeInfo>> {
    let num_rows = grid.cells.len();
    let num_cols = grid.columns.len();
    let mut merge_info: Vec<Vec<CellMergeInfo>> = (0..num_rows)
        .map(|_| {
            (0..num_cols)
                .map(|_| CellMergeInfo {
                    colspan: 1,
                    rowspan: 1,
                    covered: false,
                })
                .collect()
        })
        .collect();
    for row_idx in 0..num_rows {
        for col_idx in 0..num_cols {
            if grid.cells[row_idx][col_idx].is_empty() {
                continue;
            }
            let cell_right = grid.cells[row_idx][col_idx]
                .iter()
                .filter_map(|&idx| spans.get(idx).map(|s| s.bbox.right()))
                .fold(f32::NEG_INFINITY, f32::max);
            if cell_right == f32::NEG_INFINITY {
                continue;
            }
            let mut extra_cols = 0u32;
            for next_col in (col_idx + 1)..num_cols {
                if !grid.cells[row_idx][next_col].is_empty() {
                    break;
                }
                if cell_right > grid.columns[next_col].x_center {
                    extra_cols += 1;
                } else {
                    break;
                }
            }
            if extra_cols > 0 {
                merge_info[row_idx][col_idx].colspan = 1 + extra_cols;
                for c in 1..=(extra_cols as usize) {
                    merge_info[row_idx][col_idx + c].covered = true;
                }
            }
        }
    }
    for col_idx in 0..num_cols {
        for row_idx in 0..num_rows {
            if grid.cells[row_idx][col_idx].is_empty() || merge_info[row_idx][col_idx].covered {
                continue;
            }
            let cell_bottom = grid.cells[row_idx][col_idx]
                .iter()
                .filter_map(|&idx| spans.get(idx).map(|s| s.bbox.bottom()))
                .fold(f32::INFINITY, f32::min);
            if cell_bottom == f32::INFINITY {
                continue;
            }
            let mut extra_rows = 0u32;
            for next_row in (row_idx + 1)..num_rows {
                if !grid.cells[next_row][col_idx].is_empty() {
                    break;
                }
                if cell_bottom < grid.rows[next_row].y_center {
                    extra_rows += 1;
                } else {
                    break;
                }
            }
            if extra_rows > 0 {
                merge_info[row_idx][col_idx].rowspan = 1 + extra_rows;
                for r in 1..=(extra_rows as usize) {
                    merge_info[row_idx + r][col_idx].covered = true;
                }
            }
        }
    }
    merge_info
}

pub(super) fn detect_header_row(grid: &GridStructure, spans: &[TextSpan]) -> Option<usize> {
    if grid.cells.len() < 2 {
        return None;
    }
    let first_row_spans: Vec<&TextSpan> = grid.cells[0]
        .iter()
        .flat_map(|cell| cell.iter().filter_map(|&idx| spans.get(idx)))
        .collect();
    if first_row_spans.is_empty() {
        return None;
    }
    let data_row_spans: Vec<&TextSpan> = grid.cells[1..]
        .iter()
        .flat_map(|row| {
            row.iter()
                .flat_map(|cell| cell.iter().filter_map(|&idx| spans.get(idx)))
        })
        .collect();
    if data_row_spans.is_empty() {
        return None;
    }
    let first_row_bold_ratio = first_row_spans
        .iter()
        .filter(|s| s.font_weight.is_bold())
        .count() as f32
        / first_row_spans.len() as f32;
    let data_bold_ratio = data_row_spans
        .iter()
        .filter(|s| s.font_weight.is_bold())
        .count() as f32
        / data_row_spans.len() as f32;
    if first_row_bold_ratio > 0.5 && data_bold_ratio < 0.3 {
        return Some(0);
    }
    let first_row_avg_size: f32 =
        first_row_spans.iter().map(|s| s.font_size).sum::<f32>() / first_row_spans.len() as f32;
    let data_avg_size: f32 =
        data_row_spans.iter().map(|s| s.font_size).sum::<f32>() / data_row_spans.len() as f32;
    if first_row_avg_size > data_avg_size + 1.5 {
        return Some(0);
    }
    None
}
