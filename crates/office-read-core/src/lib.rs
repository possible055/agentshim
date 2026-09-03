#![forbid(unsafe_code)]

mod budget;
#[allow(
    clippy::pedantic,
    reason = "format parsers follow spec record layouts rather than pedantic idioms"
)]
mod cfb;
#[allow(
    clippy::pedantic,
    reason = "format parsers follow spec record layouts rather than pedantic idioms"
)]
mod core;
#[allow(
    clippy::pedantic,
    reason = "format parsers follow spec record layouts rather than pedantic idioms"
)]
mod doc;
#[allow(
    clippy::pedantic,
    reason = "format parsers follow spec record layouts rather than pedantic idioms"
)]
mod docx;
#[allow(
    clippy::pedantic,
    reason = "format parsers follow spec record layouts rather than pedantic idioms"
)]
mod ppt;
#[allow(
    clippy::pedantic,
    reason = "format parsers follow spec record layouts rather than pedantic idioms"
)]
mod pptx;
#[allow(
    clippy::pedantic,
    reason = "format parsers follow spec record layouts rather than pedantic idioms"
)]
mod xls;
#[allow(
    clippy::pedantic,
    reason = "format parsers follow spec record layouts rather than pedantic idioms"
)]
mod xlsx;

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};

pub use budget::{CancelSignal, OfficeReadLimits};
pub const CFB_SIGNATURE: [u8; 8] = cfb::CFB_SIGNATURE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OfficeFormat {
    Docx,
    Xlsx,
    Pptx,
    Doc,
    Xls,
    Ppt,
}

impl OfficeFormat {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Docx => "DOCX",
            Self::Xlsx => "XLSX",
            Self::Pptx => "PPTX",
            Self::Doc => "DOC",
            Self::Xls => "XLS",
            Self::Ppt => "PPT",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfficeLogicalCursor {
    format: OfficeFormat,
    unit_index: usize,
    offset: usize,
}

impl OfficeLogicalCursor {
    pub const fn new(format: OfficeFormat, unit_index: usize, offset: usize) -> Self {
        Self {
            format,
            unit_index,
            offset,
        }
    }

    pub const fn format(&self) -> OfficeFormat {
        self.format
    }

    pub const fn offset(&self) -> usize {
        self.offset
    }

    pub const fn unit_index(&self) -> usize {
        self.unit_index
    }
}

pub struct OfficeMarkdownChunk {
    pub format: OfficeFormat,
    pub markdown: String,
    pub next: Option<OfficeLogicalCursor>,
}

#[derive(Debug, thiserror::Error)]
pub enum OfficeReadError {
    #[error("invalid Office document during {stage}")]
    Invalid { stage: &'static str },
    #[error("unsupported Office document during {stage}")]
    Unsupported { stage: &'static str },
    #[error("Office resource limit exceeded for {resource}: limit {limit}, observed {observed}")]
    ResourceLimit {
        resource: &'static str,
        limit: u64,
        observed: u64,
    },
    #[error("Office read cancelled")]
    Cancelled,
    #[error("Office I/O failed")]
    Io(#[source] std::io::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OfficeReadErrorKind {
    Invalid,
    Unsupported,
    ResourceLimit,
    Cancelled,
    Io,
}

impl OfficeReadError {
    pub const fn kind(&self) -> OfficeReadErrorKind {
        match self {
            Self::Invalid { .. } => OfficeReadErrorKind::Invalid,
            Self::Unsupported { .. } => OfficeReadErrorKind::Unsupported,
            Self::ResourceLimit { .. } => OfficeReadErrorKind::ResourceLimit,
            Self::Cancelled => OfficeReadErrorKind::Cancelled,
            Self::Io(_) => OfficeReadErrorKind::Io,
        }
    }
}

pub struct OfficeReadDocument {
    format: OfficeFormat,
    document: ParsedDocument,
    limits: OfficeReadLimits,
    cancelled: CancelSignal,
}

enum ParsedDocument {
    Docx {
        document: Box<docx::DocxDocument>,
        headers: Vec<String>,
        footers: Vec<String>,
    },
    Xlsx {
        document: Box<xlsx::XlsxDocument>,
        units: Vec<XlsxUnit>,
    },
    Pptx(Box<pptx::PptxDocument>),
    Doc(Box<doc::DocDocument>),
    Xls {
        document: Box<xls::XlsDocument>,
        units: Vec<XlsUnit>,
    },
    Ppt(Box<ppt::PptDocument>),
}

enum XlsxUnit {
    SheetStart {
        sheet: usize,
        prose: bool,
    },
    SheetRow {
        sheet: usize,
        row: usize,
        prose: bool,
    },
    PictureAlt {
        sheet: usize,
        picture: usize,
    },
    TextShape {
        sheet: usize,
        shape: usize,
    },
    Chart(usize),
}

enum XlsUnit {
    SheetStart(usize),
    SheetRow { sheet: usize, row: usize },
}

impl OfficeReadDocument {
    pub const fn format(&self) -> OfficeFormat {
        self.format
    }

    pub fn from_file(
        mut file: File,
        hint: OfficeFormat,
        limits: OfficeReadLimits,
        cancelled: CancelSignal,
    ) -> Result<Self, OfficeReadError> {
        let length = file.metadata().map_err(OfficeReadError::Io)?.len();
        limits.check_file_bytes(length)?;
        let _scope = budget::enter(limits, cancelled.clone());
        file.seek(SeekFrom::Start(0)).map_err(OfficeReadError::Io)?;
        let format = detect_format(&mut file, hint)?;
        file.seek(SeekFrom::Start(0)).map_err(OfficeReadError::Io)?;
        let document = parse_document(file, format)?;
        budget::check_cancelled()?;
        Ok(Self {
            format,
            document,
            limits,
            cancelled,
        })
    }

    pub fn markdown_chunk(
        &mut self,
        cursor: Option<&OfficeLogicalCursor>,
        max_bytes: usize,
    ) -> Result<OfficeMarkdownChunk, OfficeReadError> {
        if max_bytes == 0 {
            return Err(OfficeReadError::ResourceLimit {
                resource: "office_output_bytes",
                limit: 0,
                observed: 1,
            });
        }
        let _scope = budget::enter(self.limits, self.cancelled.clone());
        budget::check_cancelled()?;
        let mut unit_index = cursor.map_or(0, OfficeLogicalCursor::unit_index);
        let mut offset = cursor.map_or(0, OfficeLogicalCursor::offset);
        let unit_count = self.unit_count();
        if cursor.is_some_and(|value| value.format != self.format)
            || unit_index > unit_count
            || (unit_index == unit_count && offset != 0)
        {
            return Err(OfficeReadError::Invalid { stage: "cursor" });
        }
        let mut markdown = String::with_capacity(max_bytes.min(64 * 1024));
        while unit_index < unit_count && markdown.len() < max_bytes {
            budget::check_cancelled()?;
            let unit = self.render_unit(unit_index)?;
            budget::check_cancelled()?;
            budget::check_markdown_bytes(unit.len())?;
            if offset > unit.len() || !unit.is_char_boundary(offset) {
                return Err(OfficeReadError::Invalid { stage: "cursor" });
            }
            if offset == unit.len() {
                unit_index += 1;
                offset = 0;
                continue;
            }
            let available = max_bytes - markdown.len();
            let remaining = &unit[offset..];
            if remaining.len() <= available {
                markdown.push_str(remaining);
                unit_index += 1;
                offset = 0;
                continue;
            }
            let mut take = available;
            while take > 0 && !remaining.is_char_boundary(take) {
                take -= 1;
            }
            if take == 0 {
                let character_bytes = remaining.chars().next().map_or(0, char::len_utf8);
                return Err(OfficeReadError::ResourceLimit {
                    resource: "office_output_bytes",
                    limit: max_bytes as u64,
                    observed: character_bytes as u64,
                });
            }
            markdown.push_str(&remaining[..take]);
            offset += take;
            break;
        }
        let next = (unit_index < unit_count)
            .then(|| OfficeLogicalCursor::new(self.format, unit_index, offset));
        Ok(OfficeMarkdownChunk {
            format: self.format,
            markdown,
            next,
        })
    }

    fn unit_count(&self) -> usize {
        match &self.document {
            ParsedDocument::Docx {
                document,
                headers,
                footers,
            } => headers.len() + document.body.elements.len() + footers.len(),
            ParsedDocument::Xlsx { units, .. } => units.len(),
            ParsedDocument::Pptx(document) => document.slides.len(),
            ParsedDocument::Doc(document) => document.markdown_unit_count(),
            ParsedDocument::Xls { units, .. } => units.len(),
            ParsedDocument::Ppt(document) => document.slides.len(),
        }
    }

    fn render_unit(&self, index: usize) -> Result<String, OfficeReadError> {
        let unit_count = self.unit_count();
        let mut unit = match &self.document {
            ParsedDocument::Docx {
                document,
                headers,
                footers,
            } => render_docx_unit(document, headers, footers, index),
            ParsedDocument::Xlsx { document, units } => render_xlsx_unit(document, units, index),
            ParsedDocument::Pptx(document) => document.slide_to_markdown(index).map(|markdown| {
                if index == 0 {
                    markdown
                } else {
                    format!("\n\n{markdown}")
                }
            }),
            ParsedDocument::Doc(document) => document.markdown_unit(index),
            ParsedDocument::Xls { document, units } => {
                units.get(index).and_then(|unit| match unit {
                    XlsUnit::SheetStart(sheet) => document.sheet_markdown_start(*sheet),
                    XlsUnit::SheetRow { sheet, row } => document.sheet_markdown_row(*sheet, *row),
                })
            }
            ParsedDocument::Ppt(document) => document.slide_markdown(index),
        }
        .ok_or(OfficeReadError::Invalid { stage: "cursor" })?;
        if index + 1 == unit_count && matches!(self.document, ParsedDocument::Docx { .. }) {
            unit.truncate(unit.trim_end_matches('\n').len());
        }
        Ok(unit)
    }
}

fn render_docx_unit(
    document: &docx::DocxDocument,
    headers: &[String],
    footers: &[String],
    index: usize,
) -> Option<String> {
    if let Some(header) = headers.get(index) {
        return Some(format!("{header}\n\n"));
    }
    let body_index = index.checked_sub(headers.len())?;
    if body_index < document.body.elements.len() {
        return document.markdown_body_block(body_index);
    }
    let footer_index = body_index.checked_sub(document.body.elements.len())?;
    let footer = footers.get(footer_index)?;
    let previous_ends_double_newline = if footer_index > 0 {
        false
    } else if !document.body.elements.is_empty() {
        (0..document.body.elements.len())
            .rev()
            .find_map(|body_index| {
                document
                    .markdown_body_block(body_index)
                    .filter(|unit| !unit.is_empty())
                    .map(|unit| unit.ends_with("\n\n"))
            })
            .unwrap_or(!headers.is_empty())
    } else {
        !headers.is_empty()
    };
    let mut markdown = String::new();
    if !previous_ends_double_newline {
        markdown.push_str("\n\n");
    }
    markdown.push_str(footer);
    markdown.push('\n');
    Some(markdown)
}

fn render_xlsx_unit(
    document: &xlsx::XlsxDocument,
    units: &[XlsxUnit],
    index: usize,
) -> Option<String> {
    let markdown = match units.get(index)? {
        XlsxUnit::SheetStart { sheet, prose } => document.sheet_markdown_start(*sheet, *prose)?,
        XlsxUnit::SheetRow { sheet, row, prose } => {
            return document.sheet_markdown_row(*sheet, *row, *prose);
        }
        XlsxUnit::PictureAlt { sheet, picture } => {
            let text = document
                .worksheets
                .get(*sheet)?
                .picture_alt_text
                .get(*picture)?;
            format!("> Image: {text}")
        }
        XlsxUnit::TextShape { sheet, shape } => {
            let shape = document.worksheets.get(*sheet)?.text_shapes.get(*shape)?;
            match (shape.bold, shape.italic) {
                (true, true) => format!("***{}***", shape.text),
                (true, false) => format!("**{}**", shape.text),
                (false, true) => format!("*{}*", shape.text),
                (false, false) => shape.text.clone(),
            }
        }
        XlsxUnit::Chart(chart) => {
            let text = document.chart_text.get(*chart)?;
            format!("## Chart {}\n\n{text}", chart + 1)
        }
    };
    Some(if index == 0 {
        markdown
    } else {
        format!("\n\n{markdown}")
    })
}

fn detect_format(file: &mut File, hint: OfficeFormat) -> Result<OfficeFormat, OfficeReadError> {
    let mut magic = [0_u8; 8];
    file.read_exact(&mut magic).map_err(OfficeReadError::Io)?;
    file.seek(SeekFrom::Start(0)).map_err(OfficeReadError::Io)?;
    if magic.starts_with(b"PK\x03\x04") {
        return detect_ooxml(file);
    }
    if magic == cfb::CFB_SIGNATURE {
        return detect_legacy(file);
    }
    let _ = hint;
    Err(OfficeReadError::Invalid { stage: "container" })
}

fn detect_ooxml(file: &mut File) -> Result<OfficeFormat, OfficeReadError> {
    let reader = core::opc::OpcReader::new(file.try_clone().map_err(OfficeReadError::Io)?)
        .map_err(map_core_error)?;
    let main = reader.main_document_part().map_err(map_core_error)?;
    match reader.content_types().resolve(&main) {
        Some(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        ) => Ok(OfficeFormat::Docx),
        Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml") => {
            Ok(OfficeFormat::Xlsx)
        }
        Some(
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        ) => Ok(OfficeFormat::Pptx),
        Some(_) => Err(OfficeReadError::Unsupported {
            stage: "content_types",
        }),
        None => Err(OfficeReadError::Invalid {
            stage: "content_types",
        }),
    }
}

fn detect_legacy(file: &mut File) -> Result<OfficeFormat, OfficeReadError> {
    let cfb = cfb::CfbReader::new(file.try_clone().map_err(OfficeReadError::Io)?)
        .map_err(map_cfb_error)?;
    if cfb.has_stream("WordDocument") {
        Ok(OfficeFormat::Doc)
    } else if cfb.has_stream("Workbook") || cfb.has_stream("Book") {
        Ok(OfficeFormat::Xls)
    } else if cfb.has_stream("PowerPoint Document") || cfb.has_stream("PP97_DUALSTORAGE") {
        Ok(OfficeFormat::Ppt)
    } else {
        Err(OfficeReadError::Unsupported {
            stage: "cfb_streams",
        })
    }
}

fn parse_document(file: File, format: OfficeFormat) -> Result<ParsedDocument, OfficeReadError> {
    match format {
        OfficeFormat::Docx => docx::DocxDocument::from_reader(file)
            .map(|document| {
                let (headers, footers) = document.markdown_header_footer_units();
                ParsedDocument::Docx {
                    document: Box::new(document),
                    headers,
                    footers,
                }
            })
            .map_err(|error| match error {
                docx::DocxError::Core(error) => map_core_error(error),
            }),
        OfficeFormat::Xlsx => xlsx::XlsxDocument::from_reader(file)
            .map(|document| {
                let mut units = Vec::new();
                for sheet in 0..document.worksheets.len() {
                    if document.sheet_has_markdown(sheet) {
                        let prose = document.sheet_uses_prose_markdown(sheet);
                        units.push(XlsxUnit::SheetStart { sheet, prose });
                        let first_row = usize::from(!prose);
                        for row in first_row..document.worksheets[sheet].rows.len() {
                            if !prose || document.sheet_markdown_row(sheet, row, true).is_some() {
                                units.push(XlsxUnit::SheetRow { sheet, row, prose });
                            }
                        }
                    }
                    for picture in 0..document.worksheets[sheet].picture_alt_text.len() {
                        units.push(XlsxUnit::PictureAlt { sheet, picture });
                    }
                    for shape in 0..document.worksheets[sheet].text_shapes.len() {
                        units.push(XlsxUnit::TextShape { sheet, shape });
                    }
                }
                for (chart, text) in document.chart_text.iter().enumerate() {
                    if !text.trim().is_empty() {
                        units.push(XlsxUnit::Chart(chart));
                    }
                }
                ParsedDocument::Xlsx {
                    document: Box::new(document),
                    units,
                }
            })
            .map_err(|error| match error {
                xlsx::XlsxError::Core(error) => map_core_error(error),
            }),
        OfficeFormat::Pptx => pptx::PptxDocument::from_reader(file)
            .map(|document| ParsedDocument::Pptx(Box::new(document)))
            .map_err(|error| match error {
                pptx::PptxError::Core(error) => map_core_error(error),
            }),
        OfficeFormat::Doc => doc::DocDocument::from_reader(file)
            .map(|document| ParsedDocument::Doc(Box::new(document)))
            .map_err(map_doc_error),
        OfficeFormat::Xls => xls::XlsDocument::from_reader(file)
            .map(|document| {
                let mut units = Vec::new();
                for (sheet, worksheet) in document.sheets.iter().enumerate() {
                    units.push(XlsUnit::SheetStart(sheet));
                    if worksheet.rows.iter().map(Vec::len).max().unwrap_or(0) > 0 {
                        for row in 1..worksheet.rows.len() {
                            units.push(XlsUnit::SheetRow { sheet, row });
                        }
                    }
                }
                ParsedDocument::Xls {
                    document: Box::new(document),
                    units,
                }
            })
            .map_err(map_xls_error),
        OfficeFormat::Ppt => ppt::PptDocument::from_reader(file)
            .map(|document| ParsedDocument::Ppt(Box::new(document)))
            .map_err(map_ppt_error),
    }
}

fn map_core_error(error: core::Error) -> OfficeReadError {
    match error {
        core::Error::ResourceLimit {
            resource,
            limit,
            observed,
        } => OfficeReadError::ResourceLimit {
            resource,
            limit,
            observed,
        },
        core::Error::Cancelled => OfficeReadError::Cancelled,
        core::Error::Io(error) => OfficeReadError::Io(error),
        core::Error::Unsupported(_) => OfficeReadError::Unsupported { stage: "ooxml" },
        _ => OfficeReadError::Invalid { stage: "ooxml" },
    }
}

fn map_office_error_to_core(error: OfficeReadError) -> core::Error {
    match error {
        OfficeReadError::ResourceLimit {
            resource,
            limit,
            observed,
        } => core::Error::ResourceLimit {
            resource,
            limit,
            observed,
        },
        OfficeReadError::Cancelled => core::Error::Cancelled,
        OfficeReadError::Io(error) => core::Error::Io(error),
        OfficeReadError::Unsupported { .. } => {
            core::Error::Unsupported("resource validation rejected unsupported input".to_owned())
        }
        OfficeReadError::Invalid { .. } => {
            core::Error::MalformedXml("resource validation failed".to_owned())
        }
    }
}

fn map_cfb_error(error: cfb::CfbError) -> OfficeReadError {
    match error {
        cfb::CfbError::ResourceLimit {
            resource,
            limit,
            observed,
        } => OfficeReadError::ResourceLimit {
            resource,
            limit,
            observed,
        },
        cfb::CfbError::Cancelled => OfficeReadError::Cancelled,
        cfb::CfbError::Io(error) => OfficeReadError::Io(error),
        _ => OfficeReadError::Invalid { stage: "cfb" },
    }
}

fn map_office_error_to_cfb(error: OfficeReadError) -> cfb::CfbError {
    match error {
        OfficeReadError::ResourceLimit {
            resource,
            limit,
            observed,
        } => cfb::CfbError::ResourceLimit {
            resource,
            limit,
            observed,
        },
        OfficeReadError::Cancelled => cfb::CfbError::Cancelled,
        OfficeReadError::Io(error) => cfb::CfbError::Io(error),
        OfficeReadError::Invalid { .. } | OfficeReadError::Unsupported { .. } => {
            cfb::CfbError::CorruptedStream("resource validation failed".to_owned())
        }
    }
}

fn map_doc_error(error: doc::DocError) -> OfficeReadError {
    match error {
        doc::DocError::Cfb(error) => map_cfb_error(error),
        doc::DocError::Io(error) => OfficeReadError::Io(error),
        doc::DocError::Unsupported(_) => OfficeReadError::Unsupported { stage: "doc_fib" },
        _ => OfficeReadError::Invalid { stage: "doc" },
    }
}

fn map_xls_error(error: xls::XlsError) -> OfficeReadError {
    match error {
        xls::XlsError::Cfb(error) => map_cfb_error(error),
        xls::XlsError::Io(error) => OfficeReadError::Io(error),
        xls::XlsError::UnsupportedVersion(_) => OfficeReadError::Unsupported { stage: "xls_biff" },
        _ => OfficeReadError::Invalid { stage: "xls" },
    }
}

fn map_ppt_error(error: ppt::PptError) -> OfficeReadError {
    match error {
        ppt::PptError::Cfb(error) => map_cfb_error(error),
        ppt::PptError::Io(error) => OfficeReadError::Io(error),
        ppt::PptError::ResourceLimit {
            resource,
            limit,
            observed,
        } => OfficeReadError::ResourceLimit {
            resource,
            limit,
            observed,
        },
        ppt::PptError::Cancelled => OfficeReadError::Cancelled,
        _ => OfficeReadError::Invalid { stage: "ppt" },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn oracle(document: &OfficeReadDocument) -> String {
        match &document.document {
            ParsedDocument::Docx { document, .. } => document.to_markdown(),
            ParsedDocument::Xlsx { document, .. } => document.to_markdown(),
            ParsedDocument::Pptx(document) => document.to_markdown(),
            ParsedDocument::Doc(document) => document.to_markdown(),
            ParsedDocument::Xls { document, .. } => document.to_markdown(),
            ParsedDocument::Ppt(document) => document.to_markdown(),
        }
    }

    #[test]
    fn chunks_reassemble_to_the_unlimited_format_oracle() {
        let cases = [
            ("sample.docx", OfficeFormat::Docx),
            ("sample.xlsx", OfficeFormat::Xlsx),
            ("sample.pptx", OfficeFormat::Pptx),
            ("sample.doc", OfficeFormat::Doc),
            ("sample.xls", OfficeFormat::Xls),
            ("sample.ppt", OfficeFormat::Ppt),
        ];
        for (name, format) in cases {
            for maximum in [1, 7, 23, 4_096] {
                let mut document = OfficeReadDocument::from_file(
                    File::open(fixture(name)).expect("fixture"),
                    format,
                    OfficeReadLimits::within(64 * 1024 * 1024),
                    CancelSignal::never(),
                )
                .unwrap_or_else(|error| panic!("parse {name}: {error:?}"));
                let expected = oracle(&document);
                let mut actual = String::new();
                let mut cursor = None;
                loop {
                    let chunk = document
                        .markdown_chunk(cursor.as_ref(), maximum)
                        .expect("chunk");
                    actual.push_str(&chunk.markdown);
                    cursor = chunk.next;
                    if cursor.is_none() {
                        break;
                    }
                }
                assert_eq!(actual, expected, "{name} with {maximum}-byte chunks");
            }
        }
    }
}
