use super::PptxDocument;
use super::shape::{
    GraphicContent, HyperlinkTarget, Shape, ShapePosition, Table, TextBody, TextContent,
};

impl PptxDocument {
    /// Convert the entire presentation to markdown.
    #[cfg(test)]
    pub fn to_markdown(&self) -> String {
        let mut parts = Vec::new();
        for (i, _) in self.slides.iter().enumerate() {
            if let Some(md) = self.slide_to_markdown(i) {
                parts.push(md);
            }
        }
        parts.join("\n\n")
    }

    /// Convert a single slide to markdown by index.
    pub fn slide_to_markdown(&self, index: usize) -> Option<String> {
        let slide = self.slides.get(index)?;
        let mut result = String::new();

        // Slide heading: use title placeholder text or "Slide N"
        let title = find_title_text(&slide.shapes);
        if let Some(ref title) = title {
            result.push_str(&format!("## {title}\n\n"));
        } else {
            result.push_str(&format!("## Slide {}\n\n", index + 1));
        }

        let mut entries = Vec::new();
        collect_markdown_entries(&slide.shapes, &mut entries);
        entries.sort_by(|a, b| spatial_cmp(&a.0, &b.0));

        for (_, md) in &entries {
            if crate::budget::is_cancelled() {
                break;
            }
            if !md.is_empty() {
                result.push_str(md);
                result.push_str("\n\n");
                if !crate::budget::markdown_within_limit(result.len()) {
                    break;
                }
            }
        }

        if let Some(ref notes) = slide.notes
            && !notes.is_empty()
        {
            for line in notes.lines() {
                result.push_str("> ");
                result.push_str(line);
                result.push('\n');
            }
            result.push('\n');
        }

        // Trim trailing whitespace
        let trimmed = result.trim_end().to_string();
        Some(trimmed)
    }
}

// ---------------------------------------------------------------------------
// Spatial sorting
// ---------------------------------------------------------------------------

fn spatial_cmp(a: &Option<ShapePosition>, b: &Option<ShapePosition>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.y.cmp(&b.y).then(a.x.cmp(&b.x)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

// ---------------------------------------------------------------------------
// Plain text collection
// ---------------------------------------------------------------------------

fn plain_text_from_body(body: &TextBody) -> String {
    let mut parts = Vec::new();
    for para in &body.paragraphs {
        let mut para_text = String::new();
        for content in &para.content {
            match content {
                TextContent::Run(run) => para_text.push_str(&run.text),
                TextContent::LineBreak => para_text.push('\n'),
                TextContent::Field(field) => para_text.push_str(&field.text),
            }
        }
        parts.push(para_text);
    }
    parts.join("\n")
}

// ---------------------------------------------------------------------------
// Markdown collection
// ---------------------------------------------------------------------------

fn collect_markdown_entries(shapes: &[Shape], entries: &mut Vec<(Option<ShapePosition>, String)>) {
    for shape in shapes {
        match shape {
            Shape::Auto(auto) => {
                // Skip title placeholder — already used as heading
                if auto
                    .placeholder
                    .as_ref()
                    .is_some_and(|ph| is_title_placeholder(ph.ph_type.as_deref()))
                {
                    continue;
                }
                if let Some(ref tb) = auto.text_body {
                    let md = markdown_from_body(tb);
                    if !md.is_empty() {
                        entries.push((auto.position.clone(), md));
                    }
                }
            }
            Shape::Picture(pic) => {
                if let Some(ref alt) = pic.alt_text
                    && !alt.is_empty()
                {
                    entries.push((pic.position.clone(), format!("![{alt}]()")));
                }
            }
            Shape::Group(grp) => {
                collect_markdown_entries(&grp.children, entries);
            }
            Shape::GraphicFrame(gf) => {
                if let GraphicContent::Table(ref tbl) = gf.content {
                    let md = markdown_table(tbl);
                    if !md.is_empty() {
                        entries.push((gf.position.clone(), md));
                    }
                }
            }
            Shape::Connector => {}
        }
    }
}

fn markdown_from_body(body: &TextBody) -> String {
    let mut parts = Vec::new();
    for para in &body.paragraphs {
        let text = markdown_paragraph(para);
        parts.push(text);
    }
    parts.join("\n")
}

fn markdown_paragraph(para: &super::shape::TextParagraph) -> String {
    let mut text = String::new();
    for content in &para.content {
        match content {
            TextContent::Run(run) => {
                text.push_str(&markdown_run(run));
            }
            TextContent::LineBreak => {
                text.push_str("  \n");
            }
            TextContent::Field(field) => {
                text.push_str(&field.text);
            }
        }
    }
    // Add bullet indent for outline levels > 0
    if para.level > 0 {
        let indent = "  ".repeat(para.level as usize);
        format!("{indent}- {text}")
    } else {
        text
    }
}

fn markdown_run(run: &super::shape::TextRun) -> String {
    if run.text.is_empty() {
        return String::new();
    }

    let mut text = run.text.clone();

    // Apply inline formatting
    if run.strikethrough {
        text = format!("~~{text}~~");
    }
    if run.bold == Some(true) && run.italic == Some(true) {
        text = format!("***{text}***");
    } else if run.bold == Some(true) {
        text = format!("**{text}**");
    } else if run.italic == Some(true) {
        text = format!("*{text}*");
    }

    // Apply hyperlink
    if let Some(ref link) = run.hyperlink {
        match &link.target {
            HyperlinkTarget::External(url) => {
                text = format!("[{text}]({url})");
            }
            HyperlinkTarget::Internal(target) => {
                text = format!("[{text}](#{target})");
            }
        }
    }

    text
}

fn markdown_table(table: &Table) -> String {
    if table.rows.is_empty() {
        return String::new();
    }

    let mut col_count = 0;
    let mut md_rows: Vec<Vec<String>> = Vec::new();

    for row in &table.rows {
        let mut cells = Vec::new();
        for cell in &row.cells {
            if cell.h_merge || cell.v_merge {
                continue;
            }
            let text = cell
                .text_body
                .as_ref()
                .map(|tb| {
                    // Flatten paragraphs for table cells — replace newlines with spaces
                    plain_text_from_body(tb).replace('\n', " ")
                })
                .unwrap_or_default();
            cells.push(text);
        }
        if cells.len() > col_count {
            col_count = cells.len();
        }
        md_rows.push(cells);
    }

    if col_count == 0 {
        return String::new();
    }

    let mut result = String::new();

    // Header row
    if let Some(header) = md_rows.first() {
        result.push('|');
        for (i, cell) in header.iter().enumerate() {
            result.push_str(&format!(" {cell} |"));
            if i >= col_count - 1 {
                break;
            }
        }
        // Pad if fewer cells than col_count
        for _ in header.len()..col_count {
            result.push_str("  |");
        }
        result.push('\n');

        // Separator
        result.push('|');
        for _ in 0..col_count {
            result.push_str(" --- |");
        }
        result.push('\n');
    }

    // Data rows
    for row in md_rows.iter().skip(1) {
        result.push('|');
        for (i, cell) in row.iter().enumerate() {
            result.push_str(&format!(" {cell} |"));
            if i >= col_count - 1 {
                break;
            }
        }
        for _ in row.len()..col_count {
            result.push_str("  |");
        }
        result.push('\n');
    }

    // Remove trailing newline
    if result.ends_with('\n') {
        result.pop();
    }

    result
}

// ---------------------------------------------------------------------------
// Title extraction
// ---------------------------------------------------------------------------

fn is_title_placeholder(ph_type: Option<&str>) -> bool {
    matches!(ph_type, Some("title" | "ctrTitle"))
}

fn find_title_text(shapes: &[Shape]) -> Option<String> {
    for shape in shapes {
        match shape {
            Shape::Auto(auto)
                if auto
                    .placeholder
                    .as_ref()
                    .is_some_and(|ph| is_title_placeholder(ph.ph_type.as_deref())) =>
            {
                if let Some(ref tb) = auto.text_body {
                    let text = plain_text_from_body(tb);
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
            }
            Shape::Group(grp) => {
                if let Some(title) = find_title_text(&grp.children) {
                    return Some(title);
                }
            }
            _ => {}
        }
    }
    None
}
