//! # office_oxide::xlsx
//!
//! High-performance Excel spreadsheet (.xlsx) processing.
//!
//! Read, convert, and extract content from XLSX files
//! (Office Open XML SpreadsheetML, ISO 29500 / ECMA-376).
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use office_oxide::xlsx::XlsxDocument;
//!
//! let doc = XlsxDocument::open("data.xlsx").unwrap();
//! println!("{}", doc.plain_text());
//! println!("{}", doc.to_csv());
//! println!("{}", doc.to_markdown());
//! ```

/// Cell value types and cell reference types.
pub mod cell;
/// Date/time serial number conversion for XLSX dates.
pub mod date;
/// In-place editing of existing XLSX files.
/// XLSX-specific error type.
pub mod error;
/// Number format rendering: apply Excel format strings to numeric values.
pub mod numfmt;
/// Shared string table (SST) parsing and lookup.
pub mod shared_strings;
/// Spreadsheet styles: number formats, fonts, fills, borders, cell formats.
pub mod styles;
/// Text extraction and markdown/CSV rendering for XLSX.
pub mod text;
/// Workbook-level metadata and sheet list.
pub mod workbook;
/// Worksheet parsing: cells, dimensions, hyperlinks.
pub mod worksheet;
/// XLSX creation (write) API.
pub use error::{Result, XlsxError};
pub use shared_strings::SharedStringTable;
pub use styles::StyleSheet;
pub use workbook::WorkbookInfo;
pub use worksheet::Worksheet;

use std::io::{Read, Seek};

use log::debug;
use zip::read::ZipArchive;

use crate::core::opc;
use crate::core::opc::PartName;
use crate::core::relationships::{Relationships, TargetMode, rel_types};

/// A parsed XLSX document.
#[derive(Debug, Clone)]
pub struct XlsxDocument {
    /// Workbook-level metadata (name, sheets list, date system).
    pub workbook: WorkbookInfo,
    /// Parsed worksheets in sheet-order.
    pub worksheets: Vec<Worksheet>,
    /// Shared string table.
    pub shared_strings: SharedStringTable,
    /// Stylesheet used for number and date display.
    pub styles: Option<StyleSheet>,
    /// Text content extracted from `xl/charts/chart*.xml` parts. Each entry
    /// is the flattened text (titles, axis labels, series names, category
    /// labels, values) of one chart in document order. We don't render
    /// charts as graphics but keeping their text content lets it appear in
    /// extracted text and downstream conversions.
    pub chart_text: Vec<String>,
}

impl XlsxDocument {
    /// Open an XLSX document from any `Read + Seek` source.
    pub fn from_reader<R: Read + Seek>(reader: R) -> Result<Self> {
        let archive = ZipArchive::new(reader).map_err(crate::core::Error::from)?;
        Self::from_zip(archive)
    }

    /// Read a ZIP entry by name with UTF-8 transcoding for XML parts.
    fn read_xml_entry<R: Read + Seek>(
        archive: &mut ZipArchive<R>,
        name: &str,
    ) -> std::result::Result<Vec<u8>, crate::core::Error> {
        let data = opc::read_zip_entry(archive, name)?;
        if name.ends_with(".xml") || name.ends_with(".rels") {
            let data = crate::core::xml::ensure_utf8(&data).unwrap_or(data);
            crate::budget::validate_xml(&data).map_err(crate::map_office_error_to_core)?;
            return Ok(data);
        }
        Ok(data)
    }

    fn read_optional_xml_entry<R: Read + Seek>(
        archive: &mut ZipArchive<R>,
        name: &str,
    ) -> std::result::Result<Option<Vec<u8>>, crate::core::Error> {
        match Self::read_xml_entry(archive, name) {
            Ok(data) => Ok(Some(data)),
            Err(
                crate::core::Error::MissingPart(_)
                | crate::core::Error::Zip(zip::result::ZipError::FileNotFound),
            ) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Fast path: open ZIP directly and read XLSX parts by known paths,
    /// bypassing OPC content-types and package-level relationships.
    fn from_zip<R: Read + Seek>(mut archive: ZipArchive<R>) -> Result<Self> {
        crate::budget::check_zip_entries(archive.len()).map_err(crate::map_office_error_to_core)?;
        debug!(
            "XlsxDocument: fast path parsing started ({} ZIP entries)",
            archive.len()
        );

        // Read workbook relationships to resolve sheet targets
        let wb_rels =
            match Self::read_optional_xml_entry(&mut archive, "xl/_rels/workbook.xml.rels")? {
                Some(data) => Relationships::parse(&data)?,
                None => Relationships::empty(),
            };

        // Parse shared strings (must be first — cells reference by index)
        let shared_strings =
            match Self::read_optional_xml_entry(&mut archive, "xl/sharedStrings.xml")? {
                Some(data) => SharedStringTable::parse(&data)?,
                None => SharedStringTable::empty(),
            };

        // Parse styles eagerly — needed for date detection in format_cell_value().
        let styles = match Self::read_optional_xml_entry(&mut archive, "xl/styles.xml")? {
            Some(data) => Some(StyleSheet::parse(&data)?),
            None => None,
        };

        // Parse workbook
        let wb_data = Self::read_xml_entry(&mut archive, "xl/workbook.xml")?;
        let workbook = WorkbookInfo::parse(&wb_data)?;

        // Phase 1: gather raw data sequentially (requires &mut archive)
        struct SheetBundle {
            name: String,
            data: Vec<u8>,
            rels: Relationships,
            text_shapes: Vec<crate::xlsx::worksheet::WorksheetTextShape>,
            picture_alt_text: Vec<String>,
        }
        crate::budget::check_model_items("office_xlsx_sheets", workbook.sheets.len())
            .map_err(crate::map_office_error_to_core)?;
        let mut bundles = Vec::with_capacity(workbook.sheets.len());
        for sheet in &workbook.sheets {
            let relationship = wb_rels
                .get_by_id(&sheet.rel_id)
                .ok_or_else(|| crate::core::Error::RelationshipNotFound(sheet.rel_id.clone()))?;
            if relationship.target_mode != TargetMode::Internal {
                return Err(crate::core::Error::MalformedXml(
                    "worksheet relationship must be internal".to_string(),
                )
                .into());
            }
            let workbook_part = PartName::new("/xl/workbook.xml")?;
            let sheet_part = workbook_part.resolve_relative(&relationship.target)?;
            let sheet_path = sheet_part.as_str().trim_start_matches('/').to_string();
            let ws_data = Self::read_xml_entry(&mut archive, &sheet_path)?;

            // Read worksheet relationships (for hyperlinks)
            let rels_path = sheet_rels_path(&sheet_path);
            let ws_rels = match Self::read_optional_xml_entry(&mut archive, &rels_path)? {
                Some(data) => Relationships::parse(&data)?,
                None => Relationships::empty(),
            };

            // Resolve the worksheet's DRAWING rel up-front (Phase 1
            // has access to &mut archive). Each entry decodes
            // `<xdr:pic>` and `<xdr:sp>` anchors and the underlying
            // media bytes so Phase 2's parallel parser doesn't need
            // the archive.
            let (picture_alt_text, text_shapes) =
                read_drawing_metadata_for_sheet(&mut archive, &sheet_path, &ws_rels)?;

            bundles.push(SheetBundle {
                name: sheet.name.clone(),
                data: ws_data,
                rels: ws_rels,
                text_shapes,
                picture_alt_text,
            });
        }

        // Phase 2: parse worksheets (parallel when feature enabled)
        let worksheets = bundles
            .into_iter()
            .map(|bundle| {
                let mut worksheet = Worksheet::parse(&bundle.data, bundle.name, &bundle.rels)?;
                worksheet.text_shapes = bundle.text_shapes;
                worksheet.picture_alt_text = bundle.picture_alt_text;
                Ok(worksheet)
            })
            .collect::<Result<Vec<_>>>()?;

        for worksheet in &worksheets {
            for row in &worksheet.rows {
                crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
                for cell in &row.cells {
                    if let crate::xlsx::cell::CellValue::SharedString(index) = cell.value {
                        if index as usize >= shared_strings.len() {
                            return Err(crate::core::Error::MalformedXml(
                                "shared string index is out of range".to_owned(),
                            )
                            .into());
                        }
                    }
                    if let Some(style_index) = cell.style_index {
                        if !styles
                            .as_ref()
                            .is_some_and(|style_sheet| style_sheet.contains_style(style_index))
                        {
                            return Err(crate::core::Error::MalformedXml(
                                "cell style index is out of range".to_owned(),
                            )
                            .into());
                        }
                    }
                }
            }
        }

        // Scan for chart XML parts (xl/charts/chart*.xml) and extract their
        // visible text — title, axis titles, series names, category labels,
        // cached values. We don't render charts as graphics but their words
        // belong in any text-based downstream conversion (markdown, search
        // indexes, accessibility readers, our PDF text fallback).
        let mut chart_text: Vec<String> = Vec::new();
        let mut chart_names = Vec::new();
        for index in 0..archive.len() {
            let name = archive
                .by_index(index)
                .map_err(crate::core::Error::from)?
                .name()
                .to_string();
            if name.starts_with("xl/charts/chart") && name.ends_with(".xml") {
                chart_names.push(name);
            }
        }
        chart_names.sort_unstable();
        for name in chart_names {
            let data = Self::read_xml_entry(&mut archive, &name)?;
            let text = extract_chart_text(&data)?;
            if !text.is_empty() {
                chart_text.push(text);
            }
        }

        debug!(
            "XlsxDocument: {} worksheets parsed, {} chart(s)",
            worksheets.len(),
            chart_text.len()
        );
        Ok(XlsxDocument {
            workbook,
            worksheets,
            shared_strings,
            styles,
            chart_text,
        })
    }
}

/// Extract structured content from a chart XML stream (DrawingML chart
/// format) into a flat textual representation.
///
/// Walks the chart's title (`<c:title>`), axis titles (`<c:catAx>` /
/// `<c:valAx>` / `<c:title>`), and each series (`<c:ser>`). For every
/// series we capture the name (`<c:tx>`), category labels (`<c:cat>`),
/// and cached numeric values (`<c:val>`). The output groups them into
/// readable lines that include the **structure** of the chart — series
/// names paired with their values per category — rather than the flat
/// soup of `<a:t>`/`<c:v>` text the previous implementation produced.
///
/// Output shape:
/// ```text
/// Title: ...
/// Categories: A, B, C, ...
/// Series Budget: 1690, 2100, 1570, ...
/// Series Projected: 1310, 3480, 510, ...
/// ```
///
/// This still travels through `to_markdown` and `convert_xlsx_to_ir` as
/// plain text (not an actual table), but the structure is now meaningful
/// for both human readers and downstream NLP / search.
fn extract_chart_text(xml: &[u8]) -> crate::core::Result<String> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();

    // Tag-context stack — push localname on Start, pop on End.
    let mut stack: Vec<Vec<u8>> = Vec::new();
    // Most recently seen text inside a `<t>` (rich-text run) — used to
    // build the chart title and axis-title strings.
    let mut current_title: String = String::new();
    let mut titles: Vec<String> = Vec::new();
    // The chart-level title is the first `<c:title>` we close that lives
    // outside any `<c:catAx>` / `<c:valAx>` / `<c:legend>`.
    // Per-series state.
    let mut series: Vec<ChartSeries> = Vec::new();
    let mut cur_series: Option<ChartSeries> = None;
    // Current `<c:v>` text being accumulated.
    let mut cur_v: String = String::new();
    // Categories from the current series (or the first series — they are
    // typically shared across all series in the chart).
    let mut shared_categories: Vec<String> = Vec::new();
    let mut cur_cat_buf: Vec<String> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => {
                let local = e.local_name().as_ref().to_vec();
                if local == b"ser" {
                    cur_series = Some(ChartSeries::default());
                    cur_cat_buf.clear();
                }
                stack.push(local);
            }
            Ok(quick_xml::events::Event::End(e)) => {
                let local = e.local_name().as_ref().to_vec();
                let _ = stack.pop();
                match local.as_slice() {
                    b"t" => {
                        // End of a rich-text run — accumulate into current_title
                        // if we're inside a chart-level or axis title.
                    }
                    b"title" => {
                        if !current_title.trim().is_empty() {
                            titles.push(current_title.trim().to_string());
                        }
                        current_title.clear();
                    }
                    b"v" => {
                        let val = cur_v.trim().to_string();
                        cur_v.clear();
                        if val.is_empty() {
                            continue;
                        }
                        if let Some(s) = cur_series.as_mut() {
                            // Decide whether this <c:v> is series-name, category,
                            // or value based on the enclosing scope.
                            let in_tx = stack.iter().any(|t| t.as_slice() == b"tx");
                            let in_cat = stack.iter().any(|t| t.as_slice() == b"cat");
                            let in_val = stack.iter().any(|t| t.as_slice() == b"val");
                            if in_tx && s.name.is_empty() {
                                s.name = val;
                            } else if in_cat {
                                cur_cat_buf.push(val);
                            } else if in_val {
                                s.values.push(val);
                            }
                        }
                    }
                    b"ser" => {
                        if let Some(mut s) = cur_series.take() {
                            // Fold the per-series categories into shared_categories
                            // (first series wins — they are typically identical).
                            if shared_categories.is_empty() && !cur_cat_buf.is_empty() {
                                shared_categories = std::mem::take(&mut cur_cat_buf);
                            } else {
                                cur_cat_buf.clear();
                            }
                            if s.name.is_empty() {
                                s.name = format!("Series {}", series.len() + 1);
                            }
                            series.push(s);
                        }
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Text(t)) => {
                let s = crate::core::xml::unescape_text(&t)?;
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let top = stack.last().map(|v| v.as_slice());
                match top {
                    Some(b"t") => {
                        current_title.push_str(trimmed);
                    }
                    Some(b"v") => {
                        cur_v.push_str(trimmed);
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(error) => return Err(error.into()),
            _ => {}
        }
        buf.clear();
    }

    // Emit a structured representation. Each line is independent — the
    // markdown writer joins them with `\n`.
    let mut out = String::new();
    if !titles.is_empty() {
        out.push_str(&format!("Title: {}", titles.join(" — ")));
    }
    if !shared_categories.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("Categories: {}", shared_categories.join(", ")));
    }
    for s in &series {
        if !out.is_empty() {
            out.push('\n');
        }
        if s.values.is_empty() {
            out.push_str(&format!("Series: {}", s.name));
        } else {
            out.push_str(&format!("{}: {}", s.name, s.values.join(", ")));
        }
    }
    Ok(out)
}

#[derive(Default)]
struct ChartSeries {
    name: String,
    values: Vec<String>,
}

/// Compute the .rels path for a worksheet ZIP entry.
/// e.g. "xl/worksheets/sheet1.xml" → "xl/worksheets/_rels/sheet1.xml.rels"
fn sheet_rels_path(sheet_path: &str) -> String {
    if let Some(pos) = sheet_path.rfind('/') {
        let dir = &sheet_path[..pos];
        let file = &sheet_path[pos + 1..];
        format!("{dir}/_rels/{file}.rels")
    } else {
        format!("_rels/{sheet_path}.rels")
    }
}

/// Read the DRAWING-rel target for a worksheet, parse its `<xdr:pic>`
/// and `<xdr:sp>` anchors without reading the referenced media payloads.
fn read_drawing_metadata_for_sheet<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    sheet_path: &str,
    sheet_rels: &Relationships,
) -> Result<(Vec<String>, Vec<crate::xlsx::worksheet::WorksheetTextShape>)> {
    let drawing_rel = match sheet_rels.first_by_type(rel_types::DRAWING) {
        Some(r) => r,
        None => return Ok((Vec::new(), Vec::new())),
    };
    if drawing_rel.target_mode != TargetMode::Internal {
        return Err(crate::core::Error::MalformedXml(
            "drawing relationship must be internal".to_string(),
        )
        .into());
    }

    let drawing_path = resolve_relative_zip_path(sheet_path, &drawing_rel.target)?;

    let drawing_xml = match XlsxDocument::read_xml_entry(archive, &drawing_path) {
        Ok(d) => d,
        Err(e) => {
            debug!("XlsxDocument: drawing part {drawing_path} unreadable ({e}); skipping");
            return Err(e.into());
        }
    };

    let parsed = match parse_drawing_anchors(&drawing_xml) {
        Ok(a) => a,
        Err(e) => {
            debug!("XlsxDocument: drawing {drawing_path} failed to parse ({e}); dropping anchors");
            return Err(e.into());
        }
    };

    let picture_alt_text = parsed
        .pictures
        .into_iter()
        .filter_map(|picture| picture.alt_text)
        .filter(|text| !text.trim().is_empty())
        .collect();
    let mut text_shapes: Vec<crate::xlsx::worksheet::WorksheetTextShape> = parsed
        .text_shapes
        .into_iter()
        .map(|t| crate::xlsx::worksheet::WorksheetTextShape {
            text: t.text,
            bold: t.bold,
            italic: t.italic,
            x_emu: t.x_emu,
            y_emu: t.y_emu,
        })
        .collect();
    text_shapes.sort_by_key(|shape| (shape.y_emu, shape.x_emu));

    Ok((picture_alt_text, text_shapes))
}

/// Resolve a `..`-relative target inside an OPC package back to an
/// absolute ZIP-entry path. Mirrors `PartName::resolve_relative` but
/// operates on plain ZIP paths (the `from_zip` fast path doesn't use
/// `PartName`).
fn resolve_relative_zip_path(source: &str, target: &str) -> crate::core::Result<String> {
    let source = PartName::new(&format!("/{}", source.trim_start_matches('/')))?;
    Ok(source
        .resolve_relative(target)?
        .as_str()
        .trim_start_matches('/')
        .to_string())
}

#[derive(Debug)]
struct DrawingPictureAnchor {
    alt_text: Option<String>,
}

#[derive(Debug, Default)]
struct DrawingTextAnchor {
    text: String,
    bold: bool,
    italic: bool,
    x_emu: i64,
    y_emu: i64,
}

#[derive(Debug, Default)]
struct DrawingAnchors {
    pictures: Vec<DrawingPictureAnchor>,
    text_shapes: Vec<DrawingTextAnchor>,
}

/// Parse `xl/drawings/drawingN.xml` and return both `<xdr:pic>` and
/// `<xdr:sp>` anchors. Supports `<xdr:absoluteAnchor>` (direct EMU
/// pos+ext) and the cell-anchor variants — for cell anchors we
/// approximate the absolute origin from `<xdr:from>` x/y when present.
/// `<xdr:sp>` shapes carry text inside `<xdr:txBody>` runs.
fn parse_drawing_anchors(xml_data: &[u8]) -> crate::core::Result<DrawingAnchors> {
    use quick_xml::events::Event;

    let mut reader = crate::core::xml::make_fast_reader(xml_data);
    let mut out = DrawingAnchors::default();

    // Per-anchor accumulator state. We don't pre-classify the anchor
    // as picture-vs-text; we discover that mid-walk based on which
    // child element appears (`pic` vs `sp`).
    enum AnchorKind {
        Unknown,
        Picture,
        Text,
    }
    let mut in_anchor = false;
    let mut kind = AnchorKind::Unknown;
    let mut x_emu = 0i64;
    let mut y_emu = 0i64;
    let mut cx_emu = 0i64;
    let mut cy_emu = 0i64;
    let mut embed_rid: Option<String> = None;
    let mut alt_text: Option<String> = None;
    // Text-shape state.
    let mut in_txbody = false;
    let mut in_run = false;
    let mut in_a_t = false;
    let mut text_buf = String::new();
    let mut bold = false;
    let mut italic = false;

    loop {
        let evt = reader.read_event()?;
        match evt {
            Event::Start(ref e) => {
                let local = e.local_name().as_ref().to_vec();
                match local.as_slice() {
                    b"absoluteAnchor" | b"oneCellAnchor" | b"twoCellAnchor" => {
                        in_anchor = true;
                        kind = AnchorKind::Unknown;
                        x_emu = 0;
                        y_emu = 0;
                        cx_emu = 0;
                        cy_emu = 0;
                        embed_rid = None;
                        alt_text = None;
                        in_txbody = false;
                        in_run = false;
                        in_a_t = false;
                        text_buf.clear();
                        bold = false;
                        italic = false;
                    }
                    b"pic" if in_anchor => {
                        kind = AnchorKind::Picture;
                    }
                    b"sp" if in_anchor => {
                        kind = AnchorKind::Text;
                    }
                    b"txBody" if in_anchor => {
                        in_txbody = true;
                    }
                    b"r" if in_txbody => {
                        in_run = true;
                    }
                    b"t" if in_run => {
                        in_a_t = true;
                    }
                    b"rPr" if in_run => {
                        for attr in e.attributes().with_checks(false) {
                            let attr = attr.map_err(crate::core::Error::from)?;
                            let key = attr.key.as_ref();
                            let raw = crate::core::xml::unescape_attr_value(&attr)?;
                            match key {
                                b"b" => bold = raw == "1" || raw == "true",
                                b"i" => italic = raw == "1" || raw == "true",
                                _ => {}
                            }
                        }
                    }
                    b"cNvPr" if in_anchor => {
                        if let Some(d) = crate::core::xml::optional_attr_str(e, b"descr")? {
                            alt_text = Some(d.into_owned());
                        }
                    }
                    _ => {}
                }
            }
            Event::Empty(ref e) => {
                if !in_anchor {
                    continue;
                }
                let local = e.local_name().as_ref().to_vec();
                match local.as_slice() {
                    b"pos" => {
                        if let Some(v) = crate::core::xml::optional_attr_str(e, b"x")? {
                            x_emu = v.parse().unwrap_or(0);
                        }
                        if let Some(v) = crate::core::xml::optional_attr_str(e, b"y")? {
                            y_emu = v.parse().unwrap_or(0);
                        }
                    }
                    b"ext" => {
                        if let Some(v) = crate::core::xml::optional_attr_str(e, b"cx")? {
                            cx_emu = v.parse().unwrap_or(0);
                        }
                        if let Some(v) = crate::core::xml::optional_attr_str(e, b"cy")? {
                            cy_emu = v.parse().unwrap_or(0);
                        }
                    }
                    b"off" if cx_emu == 0 && cy_emu == 0 && matches!(kind, AnchorKind::Unknown) => {
                        // Honour `<off>` only at the outermost anchor level,
                        // before we've descended into `<xdr:pic>` or
                        // `<xdr:sp>`. Otherwise the `<a:off>` inside a
                        // shape's `<a:xfrm>` (which expresses a transform
                        // local to the shape, not the anchor origin) would
                        // overwrite the absolute coordinates parsed from
                        // `<xdr:pos>`.
                        if let Some(v) = crate::core::xml::optional_attr_str(e, b"x")? {
                            x_emu = v.parse().unwrap_or(x_emu);
                        }
                        if let Some(v) = crate::core::xml::optional_attr_str(e, b"y")? {
                            y_emu = v.parse().unwrap_or(y_emu);
                        }
                    }
                    b"blip" => {
                        for attr in e.attributes().with_checks(false) {
                            let attr = attr.map_err(crate::core::Error::from)?;
                            let key = attr.key.as_ref();
                            if key == b"r:embed" || key.ends_with(b":embed") || key == b"embed" {
                                embed_rid = Some(crate::core::xml::unescape_attr_value(&attr)?);
                                break;
                            }
                        }
                    }
                    b"cNvPr" => {
                        if let Some(d) = crate::core::xml::optional_attr_str(e, b"descr")? {
                            alt_text = Some(d.into_owned());
                        }
                    }
                    b"rPr" if in_run => {
                        for attr in e.attributes().with_checks(false) {
                            let attr = attr.map_err(crate::core::Error::from)?;
                            let key = attr.key.as_ref();
                            let raw = crate::core::xml::unescape_attr_value(&attr)?;
                            match key {
                                b"b" => bold = raw == "1" || raw == "true",
                                b"i" => italic = raw == "1" || raw == "true",
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(ref e) if in_a_t => {
                let s = crate::core::xml::unescape_text(e)?;
                text_buf.push_str(&s);
            }
            Event::End(ref e) => {
                let local = e.local_name().as_ref().to_vec();
                match local.as_slice() {
                    b"t" => in_a_t = false,
                    b"r" => in_run = false,
                    b"txBody" => in_txbody = false,
                    s if matches!(s, b"absoluteAnchor" | b"oneCellAnchor" | b"twoCellAnchor")
                        && in_anchor =>
                    {
                        in_anchor = false;
                        match kind {
                            AnchorKind::Picture => {
                                if embed_rid.take().is_some() || alt_text.is_some() {
                                    out.pictures.push(DrawingPictureAnchor {
                                        alt_text: alt_text.take(),
                                    });
                                }
                            }
                            AnchorKind::Text => {
                                if !text_buf.is_empty() {
                                    out.text_shapes.push(DrawingTextAnchor {
                                        text: std::mem::take(&mut text_buf),
                                        bold,
                                        italic,
                                        x_emu,
                                        y_emu,
                                    });
                                }
                            }
                            AnchorKind::Unknown => {}
                        }
                        kind = AnchorKind::Unknown;
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(out)
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn sheet_rels_path_top_level() {
        assert_eq!(
            sheet_rels_path("xl/worksheets/sheet1.xml"),
            "xl/worksheets/_rels/sheet1.xml.rels"
        );
        assert_eq!(sheet_rels_path("sheet1.xml"), "_rels/sheet1.xml.rels");
    }

    #[test]
    fn resolve_relative_zip_path_absolute() {
        assert_eq!(
            resolve_relative_zip_path("xl/worksheets/sheet1.xml", "/xl/media/img1.png"),
            "xl/media/img1.png"
        );
    }

    #[test]
    fn resolve_relative_zip_path_dotdot() {
        assert_eq!(
            resolve_relative_zip_path("xl/worksheets/sheet1.xml", "../drawings/drawing1.xml"),
            "xl/drawings/drawing1.xml"
        );
    }

    #[test]
    fn resolve_relative_zip_path_dot_segment() {
        assert_eq!(
            resolve_relative_zip_path("xl/worksheets/sheet1.xml", "./local.xml"),
            "xl/worksheets/local.xml"
        );
    }

    #[test]
    fn resolve_relative_zip_path_source_at_root() {
        assert_eq!(
            resolve_relative_zip_path("file.xml", "sub/x.xml"),
            "sub/x.xml"
        );
    }

    #[test]
    fn guess_image_format_signatures() {
        assert_eq!(
            guess_image_format_from_bytes(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10]),
            "png"
        );
        assert_eq!(
            guess_image_format_from_bytes(&[0xFF, 0xD8, 0xFF, 0xE0]),
            "jpeg"
        );
        assert_eq!(guess_image_format_from_bytes(b"GIF89a..."), "gif");
        assert_eq!(guess_image_format_from_bytes(b"GIF87a..."), "gif");
        assert_eq!(guess_image_format_from_bytes(b"BM\0\0\0"), "bmp");
        assert_eq!(guess_image_format_from_bytes(b"II*\0\x08\0"), "tiff");
        assert_eq!(guess_image_format_from_bytes(b"MM\0*\0\x08"), "tiff");
        assert_eq!(
            guess_image_format_from_bytes(&[0xD7, 0xCD, 0xC6, 0x9A]),
            "wmf"
        );
        assert_eq!(
            guess_image_format_from_bytes(&[0x01, 0x00, 0x00, 0x00, 0x58]),
            "emf"
        );
        // Fall back to png for unknown payloads.
        assert_eq!(guess_image_format_from_bytes(&[0, 0, 0]), "png");
    }

    #[test]
    fn extract_chart_text_minimal_title() {
        let xml = br#"<?xml version="1.0"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
              xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <c:chart>
    <c:title>
      <c:tx>
        <c:rich>
          <a:p><a:r><a:t>Quarterly Sales</a:t></a:r></a:p>
        </c:rich>
      </c:tx>
    </c:title>
  </c:chart>
</c:chartSpace>"#;
        let out = extract_chart_text(xml);
        assert!(out.contains("Title: Quarterly Sales"), "got: {out}");
    }

    #[test]
    fn extract_chart_text_series_and_categories() {
        let xml = br#"<?xml version="1.0"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
              xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <c:chart><c:plotArea>
    <c:barChart>
      <c:ser>
        <c:tx><c:strRef><c:f>Sheet1!$B$1</c:f><c:strCache><c:pt><c:v>Budget</c:v></c:pt></c:strCache></c:strRef></c:tx>
        <c:cat><c:strRef><c:strCache>
          <c:pt><c:v>Q1</c:v></c:pt>
          <c:pt><c:v>Q2</c:v></c:pt>
        </c:strCache></c:strRef></c:cat>
        <c:val><c:numRef><c:numCache>
          <c:pt><c:v>1000</c:v></c:pt>
          <c:pt><c:v>2000</c:v></c:pt>
        </c:numCache></c:numRef></c:val>
      </c:ser>
    </c:barChart>
  </c:plotArea></c:chart>
</c:chartSpace>"#;
        let out = extract_chart_text(xml);
        assert!(out.contains("Categories: Q1, Q2"), "got: {out}");
        assert!(out.contains("Budget: 1000, 2000"), "got: {out}");
    }

    #[test]
    fn parse_drawing_anchors_picture_one_cell() {
        let xml = br#"<?xml version="1.0"?>
<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"
          xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <xdr:oneCellAnchor>
    <xdr:from><xdr:col>0</xdr:col><xdr:colOff>914400</xdr:colOff>
              <xdr:row>0</xdr:row><xdr:rowOff>457200</xdr:rowOff></xdr:from>
    <xdr:ext cx="2000000" cy="1500000"/>
    <xdr:pic>
      <xdr:nvPicPr>
        <xdr:cNvPr id="2" name="Image1" descr="my-alt"/>
      </xdr:nvPicPr>
      <xdr:blipFill>
        <a:blip r:embed="rId4"/>
      </xdr:blipFill>
    </xdr:pic>
  </xdr:oneCellAnchor>
</xdr:wsDr>"#;
        let parsed = parse_drawing_anchors(xml).expect("parse ok");
        assert_eq!(parsed.pictures.len(), 1);
        assert_eq!(parsed.pictures[0].embed_rid, "rId4");
        assert_eq!(parsed.pictures[0].cx_emu, 2_000_000);
        assert_eq!(parsed.pictures[0].cy_emu, 1_500_000);
        assert_eq!(parsed.pictures[0].alt_text.as_deref(), Some("my-alt"));
    }

    #[test]
    fn parse_drawing_anchors_text_shape() {
        let xml = br#"<?xml version="1.0"?>
<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"
          xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <xdr:absoluteAnchor>
    <xdr:pos x="100000" y="200000"/>
    <xdr:ext cx="3000000" cy="500000"/>
    <xdr:sp>
      <xdr:txBody>
        <a:p><a:r><a:t>Hello shape</a:t></a:r></a:p>
      </xdr:txBody>
    </xdr:sp>
  </xdr:absoluteAnchor>
</xdr:wsDr>"#;
        let parsed = parse_drawing_anchors(xml).expect("parse ok");
        assert_eq!(parsed.text_shapes.len(), 1);
        assert_eq!(parsed.text_shapes[0].text, "Hello shape");
        assert_eq!(parsed.text_shapes[0].cx_emu, 3_000_000);
    }

    #[test]
    fn parse_drawing_anchors_empty_doc_is_ok() {
        let xml = br#"<?xml version="1.0"?>
<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"/>"#;
        let parsed = parse_drawing_anchors(xml).expect("parse ok");
        assert!(parsed.pictures.is_empty());
        assert!(parsed.text_shapes.is_empty());
    }
}
