use super::*;

impl MarkdownOutputConverter {
    /// Render a Table as a markdown table string.
    ///
    /// Normalizes column counts so every row has the same number of pipe-delimited
    /// cells. Without this, markdown parsers silently drop trailing cells from
    /// short rows, which causes data loss (e.g. "CERTIFICATE NO.: 403852" missing
    /// from converted output).
    pub(super) fn render_table_markdown(
        &self,
        table: &Table,
        config: &TextPipelineConfig,
    ) -> String {
        if table.rows.is_empty() {
            return String::new();
        }

        let mut output = String::new();

        // Determine header row index - use first row if has_header, or first is_header row
        let header_end = if table.has_header {
            table.rows.iter().position(|r| !r.is_header).unwrap_or(1)
        } else {
            // Treat first row as header for markdown (markdown requires a header row)
            1
        };

        // Find the maximum effective column count across all rows.
        // Each cell contributes `colspan` columns (default 1).
        let max_cols = table
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|c| c.colspan.max(1) as usize)
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0);

        for (row_idx, row) in table.rows.iter().enumerate() {
            output.push('|');
            let mut cols_written: usize = 0;
            for cell in &row.cells {
                output.push(' ');

                // Render bold/italic from span metadata when available;
                // fall back to plain text for cells without span info.
                let cell_text = if !cell.spans.is_empty() {
                    let mut cell_md = String::new();
                    let mut active_bold = false;
                    let mut active_italic = false;

                    // Order per-span emit: close-old-markers → inter-span
                    // space → open-new-markers → text. This keeps whitespace
                    // OUTSIDE emphasis delimiters, which CommonMark requires
                    // (`** text**` and `**text **` are both rejected as
                    // literal asterisks by strict renderers).
                    for (i, span) in cell.spans.iter().enumerate() {
                        // A Markdown header row is already rendered bold by
                        // readers (via the `|---|` separator beneath it), so
                        // explicit `**` in a header cell is redundant and
                        // diverges from the conventional rendering
                        // ("| Region |", not "| **Region** |"). Suppress bold
                        // in the header row ONLY when the table actually has
                        // data rows beneath it — a single-row table is all
                        // "header", so its emphasis is real content and must be
                        // kept. Data cells always keep their bold.
                        let has_data_rows = table.rows.len() > header_end;
                        let is_header = row_idx < header_end && has_data_rows;
                        let is_bold = !is_header && self.is_bold_raw(span, config);
                        let is_italic = span.is_italic;
                        let formatting_changed =
                            is_bold != active_bold || is_italic != active_italic;

                        if formatting_changed {
                            if active_italic {
                                cell_md.push('*');
                            }
                            if active_bold {
                                cell_md.push_str("**");
                            }
                        }

                        if i > 0 {
                            let prev = &cell.spans[i - 1];
                            // Insert an inter-span space when there is a visible
                            // gap OR when the word-boundary detector already
                            // determined this span begins a new word (splitting a
                            // fused run). The cell path previously used the gap
                            // alone and so glued tight-set word pairs like
                            // "Value"+"Aktien" -> "ValueAktien" that the main text
                            // path separates via that same boundary metadata.
                            let has_gap =
                                super::has_horizontal_gap(prev, span) || span.split_boundary_before;
                            let already_has_space =
                                cell_md.ends_with(' ') || span.text.starts_with(' ');
                            if has_gap && !already_has_space {
                                cell_md.push(' ');
                            }
                        }

                        if formatting_changed {
                            if is_bold {
                                cell_md.push_str("**");
                            }
                            if is_italic {
                                cell_md.push('*');
                            }
                            active_bold = is_bold;
                            active_italic = is_italic;
                        }

                        // Apply column-spanning-decimal split (issue 487
                        // nougat_018): sailing-score cells emitted as a
                        // single Tj "1.10" with sparse char_widths split
                        // into two tokens "1 10".
                        let mut processed_text = String::new();
                        crate::document::PdfDocument::push_span_text(&mut processed_text, span);
                        let mut text = processed_text.replace('|', "\\|").replace('\n', " ");
                        let just_opened = is_bold || is_italic;
                        if just_opened && (cell_md.ends_with("**") || cell_md.ends_with('*')) {
                            while text.starts_with(' ') {
                                text.remove(0);
                            }
                        }
                        cell_md.push_str(&text);
                    }

                    // Final close: CommonMark forbids whitespace adjacent
                    // to closing markers; strip it before the markers and
                    // re-append after.
                    if active_italic || active_bold {
                        let content_end = cell_md.trim_end().len();
                        let trailing = cell_md[content_end..].to_string();
                        cell_md.truncate(content_end);
                        if active_italic {
                            cell_md.push('*');
                        }
                        if active_bold {
                            cell_md.push_str("**");
                        }
                        cell_md.push_str(&trailing);
                    }

                    cell_md
                } else {
                    cell.text.replace('|', "\\|").replace('\n', " ")
                };

                output.push_str(cell_text.trim());
                output.push(' ');
                // Handle colspan by adding extra | separators
                let span = cell.colspan.max(1) as usize;
                for _ in 1..span {
                    output.push_str("| ");
                }
                output.push('|');
                cols_written += span;
            }
            // Pad short rows with empty cells so every row has `max_cols` columns.
            for _ in cols_written..max_cols {
                output.push_str(" |");
            }
            output.push('\n');

            // Add header separator after header rows
            if row_idx + 1 == header_end {
                output.push('|');
                // Separator must also match max_cols
                let header_cols: usize = row.cells.iter().map(|c| c.colspan.max(1) as usize).sum();
                for _ in 0..max_cols.max(header_cols) {
                    output.push_str("---|");
                }
                output.push('\n');
            }
        }

        output
    }

    /// Resolve bold emphasis for a raw TextSpan honoring config.
    pub(super) fn is_bold_raw(
        &self,
        span: &crate::layout::TextSpan,
        config: &TextPipelineConfig,
    ) -> bool {
        use crate::pipeline::config::BoldMarkerBehavior;
        match span.font_weight {
            FontWeight::Bold | FontWeight::Black | FontWeight::ExtraBold | FontWeight::SemiBold => {
                match config.output.bold_marker_behavior {
                    BoldMarkerBehavior::Aggressive => true,
                    BoldMarkerBehavior::Conservative => {
                        span.text.chars().any(|c| !c.is_whitespace())
                    }
                }
            }
            _ => false,
        }
    }
}
