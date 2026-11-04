use quick_xml::events::Event;

use crate::core::relationships::{Relationships, TargetMode};
use crate::core::xml;

use super::shape::{
    AutoShape, GraphicContent, GraphicFrame, GroupShape, HyperlinkInfo, HyperlinkTarget,
    PictureShape, PlaceholderInfo, Shape, ShapePosition, Table, TableCell, TableRow, TextBody,
    TextContent, TextField, TextParagraph, TextRun,
};

type CoreResult<T> = crate::core::Result<T>;

fn charge_model_items(resource: &'static str, count: usize) -> CoreResult<()> {
    crate::budget::charge_model_items(resource, count).map_err(crate::map_office_error_to_core)
}

fn charge_model_text(resource: &'static str, bytes: usize) -> CoreResult<()> {
    crate::budget::charge_model_text(resource, bytes).map_err(crate::map_office_error_to_core)
}

type RunProps = (Option<bool>, Option<bool>, bool, Option<HyperlinkInfo>);

/// A parsed PPTX slide.
#[derive(Debug, Clone)]
pub struct Slide {
    /// All top-level shapes on this slide.
    pub shapes: Vec<Shape>,
    /// Speaker notes text, if a notes slide is present.
    pub notes: Option<String>,
}

/// Create a fast reader that does NOT trim text content.
fn make_content_reader(xml_data: &[u8]) -> quick_xml::Reader<&[u8]> {
    let mut reader = quick_xml::Reader::from_reader(xml_data);
    reader.config_mut().check_end_names = false;
    reader.config_mut().check_comments = false;
    reader
}

impl Slide {
    /// Parse a slide from its XML data.
    pub(crate) fn parse(xml_data: &[u8], rels: &Relationships) -> CoreResult<Self> {
        let mut reader = make_content_reader(xml_data);
        let mut shapes = Vec::new();

        loop {
            crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
            match reader.read_event()? {
                Event::Start(ref e) if e.local_name().as_ref() == b"spTree" => {
                    shapes = parse_shape_tree(&mut reader, rels)?;
                }
                Event::Eof => break,
                _ => {}
            }
        }

        Ok(Slide {
            shapes,
            notes: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Shape tree parsing
// ---------------------------------------------------------------------------

fn parse_shape_tree(
    reader: &mut quick_xml::Reader<&[u8]>,
    rels: &Relationships,
) -> CoreResult<Vec<Shape>> {
    let mut shapes = Vec::new();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"sp" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    shapes.push(parse_auto_shape(reader, rels)?);
                }
                b"pic" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    shapes.push(parse_picture(reader)?);
                }
                b"grpSp" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    shapes.push(parse_group_shape(reader, rels)?);
                }
                b"graphicFrame" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    shapes.push(parse_graphic_frame(reader, rels)?);
                }
                b"cxnSp" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    shapes.push(parse_connector(reader)?);
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::End(ref e) if e.local_name().as_ref() == b"spTree" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(shapes)
}

// ---------------------------------------------------------------------------
// AutoShape (p:sp)
// ---------------------------------------------------------------------------

fn parse_auto_shape(
    reader: &mut quick_xml::Reader<&[u8]>,
    rels: &Relationships,
) -> CoreResult<Shape> {
    let mut position = None;
    let mut text_body = None;
    let mut placeholder = None;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"nvSpPr" => {
                    placeholder = parse_nv_common_props(reader)?;
                }
                b"spPr" => {
                    position = parse_shape_properties(reader)?;
                }
                b"txBody" => {
                    text_body = Some(parse_text_body(reader, rels)?);
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::End(ref e) if e.local_name().as_ref() == b"sp" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(Shape::Auto(AutoShape {
        position,
        text_body,
        placeholder,
    }))
}

// ---------------------------------------------------------------------------
// PictureShape (p:pic)
// ---------------------------------------------------------------------------

fn parse_picture(reader: &mut quick_xml::Reader<&[u8]>) -> CoreResult<Shape> {
    let mut alt_text = None;
    let mut position = None;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"nvPicPr" => {
                    alt_text = parse_nv_pic_props(reader)?;
                    if let Some(text) = &alt_text {
                        charge_model_text("office_pptx_text_bytes", text.len())?;
                    }
                }
                b"blipFill" => {
                    xml::skip_element_fast(reader)?;
                }
                b"spPr" => {
                    position = parse_shape_properties(reader)?;
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::End(ref e) if e.local_name().as_ref() == b"pic" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(Shape::Picture(PictureShape { alt_text, position }))
}

// ---------------------------------------------------------------------------
// GroupShape (p:grpSp)
// ---------------------------------------------------------------------------

fn parse_group_shape(
    reader: &mut quick_xml::Reader<&[u8]>,
    rels: &Relationships,
) -> CoreResult<Shape> {
    let mut children = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"nvGrpSpPr" => {
                    xml::skip_element_fast(reader)?;
                }
                b"grpSpPr" => {
                    xml::skip_element_fast(reader)?;
                }
                b"sp" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    children.push(parse_auto_shape(reader, rels)?);
                }
                b"pic" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    children.push(parse_picture(reader)?);
                }
                b"grpSp" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    children.push(parse_group_shape(reader, rels)?);
                }
                b"graphicFrame" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    children.push(parse_graphic_frame(reader, rels)?);
                }
                b"cxnSp" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    children.push(parse_connector(reader)?);
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::End(ref e) if e.local_name().as_ref() == b"grpSp" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(Shape::Group(GroupShape { children }))
}

// ---------------------------------------------------------------------------
// GraphicFrame (p:graphicFrame)
// ---------------------------------------------------------------------------

fn parse_graphic_frame(
    reader: &mut quick_xml::Reader<&[u8]>,
    rels: &Relationships,
) -> CoreResult<Shape> {
    let mut position = None;
    let mut content = GraphicContent::Unknown;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => {
                match e.local_name().as_ref() {
                    b"nvGraphicFramePr" => {
                        xml::skip_element_fast(reader)?;
                    }
                    b"xfrm" => {
                        position = parse_xfrm(reader)?;
                    }
                    // <a:graphic> is a wrapper — keep parsing to find <a:graphicData>
                    b"graphic" => {}
                    b"graphicData" => {
                        let uri = xml::optional_attr_str(e, b"uri")?;
                        if uri.as_deref()
                            == Some("http://schemas.openxmlformats.org/drawingml/2006/table")
                        {
                            content = parse_graphic_data_table(reader, rels)?;
                        } else {
                            xml::skip_element_fast(reader)?;
                        }
                    }
                    _ => {
                        xml::skip_element_fast(reader)?;
                    }
                }
            }
            Event::End(ref e) if e.local_name().as_ref() == b"graphicFrame" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(Shape::GraphicFrame(GraphicFrame { position, content }))
}

fn parse_graphic_data_table(
    reader: &mut quick_xml::Reader<&[u8]>,
    rels: &Relationships,
) -> CoreResult<GraphicContent> {
    loop {
        match reader.read_event()? {
            Event::Start(ref e) if e.local_name().as_ref() == b"tbl" => {
                let table = parse_table(reader, rels)?;
                // Skip to end of graphicData
                skip_to_end_of(reader, b"graphicData")?;
                return Ok(GraphicContent::Table(table));
            }
            Event::End(ref e) if e.local_name().as_ref() == b"graphicData" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(GraphicContent::Unknown)
}

/// Skip remaining events until the end tag for the given element.
fn skip_to_end_of(reader: &mut quick_xml::Reader<&[u8]>, local: &[u8]) -> CoreResult<()> {
    let mut depth = 1u32;
    loop {
        match reader.read_event()? {
            Event::Start(_) => depth += 1,
            Event::End(ref e) => {
                depth -= 1;
                if depth == 0 && e.local_name().as_ref() == local {
                    return Ok(());
                }
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// ConnectorShape (p:cxnSp)
// ---------------------------------------------------------------------------

fn parse_connector(reader: &mut quick_xml::Reader<&[u8]>) -> CoreResult<Shape> {
    xml::skip_element_fast(reader)?;
    Ok(Shape::Connector)
}

// ---------------------------------------------------------------------------
// Non-visual property parsing helpers
// ---------------------------------------------------------------------------

/// Parse `p:nvSpPr` → (id, name, alt_text, placeholder)
///
/// Structure:
/// ```xml
/// <p:nvSpPr>
///   <p:cNvPr id="4" name="Title 1" descr="Alt text"/>
///   <p:cNvSpPr/>
///   <p:nvPr><p:ph type="title"/></p:nvPr>
/// </p:nvSpPr>
/// ```
fn parse_nv_common_props(
    reader: &mut quick_xml::Reader<&[u8]>,
) -> CoreResult<Option<PlaceholderInfo>> {
    let mut placeholder = None;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => {
                {
                    match e.local_name().as_ref() {
                        b"cNvPr" => {
                            xml::skip_element_fast(reader)?;
                        }
                        // p:nvPr contains p:ph — don't skip, keep parsing
                        b"nvPr" => {}
                        _ => {
                            xml::skip_element_fast(reader)?;
                        }
                    }
                }
            }
            Event::Empty(ref e) => match e.local_name().as_ref() {
                b"cNvPr" => {}
                b"ph" => {
                    let ph_type = xml::optional_attr_str(e, b"type")?.map(|v| v.into_owned());
                    if let Some(value) = &ph_type {
                        charge_model_text("office_pptx_text_bytes", value.len())?;
                    }
                    placeholder = Some(PlaceholderInfo { ph_type });
                }
                _ => {}
            },
            Event::End(ref e) if e.local_name().as_ref() == b"nvSpPr" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(placeholder)
}

/// Parse `p:nvPicPr` → (id, name, alt_text)
fn parse_nv_pic_props(reader: &mut quick_xml::Reader<&[u8]>) -> CoreResult<Option<String>> {
    let mut alt_text = None;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) | Event::Empty(ref e) if e.local_name().as_ref() == b"cNvPr" => {
                alt_text = xml::optional_attr_str(e, b"descr")?.map(|v| v.into_owned());
            }
            Event::End(ref e) if e.local_name().as_ref() == b"nvPicPr" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(alt_text)
}

// ---------------------------------------------------------------------------
// Shape properties (a:xfrm within p:spPr or p:grpSpPr)
// ---------------------------------------------------------------------------

/// Parse `p:spPr` → extract position from `a:xfrm`.
fn parse_shape_properties(
    reader: &mut quick_xml::Reader<&[u8]>,
) -> CoreResult<Option<ShapePosition>> {
    let mut position = None;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) if e.local_name().as_ref() == b"xfrm" => {
                position = Some(parse_xfrm_contents(reader)?);
            }
            Event::End(ref e) if e.local_name().as_ref() == b"spPr" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(position)
}

/// Parse `p:xfrm` (used in graphicFrame) → extract position.
fn parse_xfrm(reader: &mut quick_xml::Reader<&[u8]>) -> CoreResult<Option<ShapePosition>> {
    Ok(Some(parse_xfrm_contents(reader)?))
}

/// Parse the contents of an `a:xfrm` or `p:xfrm` element: `<a:off x y/>`, `<a:ext cx cy/>`.
fn parse_xfrm_contents(reader: &mut quick_xml::Reader<&[u8]>) -> CoreResult<ShapePosition> {
    let mut x = 0i64;
    let mut y = 0i64;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) | Event::Empty(ref e) => {
                if e.local_name().as_ref() == b"off" {
                    x = xml::optional_attr_str(e, b"x")?
                        .map(|v| v.parse::<i64>().map_err(crate::core::Error::from))
                        .transpose()?
                        .unwrap_or(0);
                    y = xml::optional_attr_str(e, b"y")?
                        .map(|v| v.parse::<i64>().map_err(crate::core::Error::from))
                        .transpose()?
                        .unwrap_or(0);
                }
            }
            Event::End(_) => {
                // End of xfrm
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(ShapePosition { x, y })
}

// ---------------------------------------------------------------------------
// Text body parsing (DrawingML a: namespace)
// ---------------------------------------------------------------------------

/// Parse `<p:txBody>` or `<a:txBody>`.
fn parse_text_body(
    reader: &mut quick_xml::Reader<&[u8]>,
    rels: &Relationships,
) -> CoreResult<TextBody> {
    let mut paragraphs = Vec::new();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"p" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    paragraphs.push(parse_text_paragraph(reader, rels)?);
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::End(_) => {
                // End of txBody
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(TextBody { paragraphs })
}

/// Parse `<a:p>`.
fn parse_text_paragraph(
    reader: &mut quick_xml::Reader<&[u8]>,
    rels: &Relationships,
) -> CoreResult<TextParagraph> {
    let mut level = 0u32;
    let mut content = Vec::new();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"pPr" => {
                    level = xml::optional_attr_str(e, b"lvl")?
                        .map(|v| v.parse::<u32>().map_err(crate::core::Error::from))
                        .transpose()?
                        .unwrap_or(0);
                    xml::skip_element_fast(reader)?;
                }
                b"r" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    content.push(TextContent::Run(parse_text_run(reader, rels)?));
                }
                b"br" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    content.push(TextContent::LineBreak);
                    xml::skip_element_fast(reader)?;
                }
                b"fld" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    content.push(TextContent::Field(parse_text_field(reader, e)?));
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::Empty(ref e) => match e.local_name().as_ref() {
                b"pPr" => {
                    level = xml::optional_attr_str(e, b"lvl")?
                        .map(|v| v.parse::<u32>().map_err(crate::core::Error::from))
                        .transpose()?
                        .unwrap_or(0);
                }
                b"br" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    content.push(TextContent::LineBreak);
                }
                _ => {}
            },
            Event::End(ref e) if e.local_name().as_ref() == b"p" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(TextParagraph { level, content })
}

/// Parse `<a:r>` text run.
fn parse_text_run(
    reader: &mut quick_xml::Reader<&[u8]>,
    rels: &Relationships,
) -> CoreResult<TextRun> {
    let mut text = String::new();
    let mut bold = None;
    let mut italic = None;
    let mut strikethrough = false;
    let mut hyperlink = None;

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"rPr" => {
                    let props = parse_run_properties(reader, e, rels)?;
                    bold = props.0;
                    italic = props.1;
                    strikethrough = props.2;
                    hyperlink = props.3;
                }
                b"t" => {
                    text = xml::read_text_content_fast(reader)?;
                    charge_model_text("office_pptx_text_bytes", text.len())?;
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::Empty(ref e) if e.local_name().as_ref() == b"rPr" => {
                let props = parse_run_properties_empty(e, rels)?;
                bold = props.0;
                italic = props.1;
                strikethrough = props.2;
                hyperlink = props.3;
            }
            Event::End(ref e) if e.local_name().as_ref() == b"r" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(TextRun {
        text,
        bold,
        italic,
        strikethrough,
        hyperlink,
    })
}

/// Parse run properties from an `<a:rPr>` Start element (has children like hlinkClick).
fn parse_run_properties(
    reader: &mut quick_xml::Reader<&[u8]>,
    start: &quick_xml::events::BytesStart,
    rels: &Relationships,
) -> CoreResult<RunProps> {
    let bold = parse_bool_attr(start, b"b")?;
    let italic = parse_bool_attr(start, b"i")?;
    let strike = xml::optional_attr_str(start, b"strike")?;
    let strikethrough = strike.as_deref().is_some_and(|v| v != "noStrike");
    let mut hyperlink = None;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => {
                if e.local_name().as_ref() == b"hlinkClick" {
                    hyperlink = parse_hlink_click(e, rels)?;
                }
            }
            Event::Empty(ref e) => {
                if e.local_name().as_ref() == b"hlinkClick" {
                    hyperlink = parse_hlink_click(e, rels)?;
                }
            }
            Event::End(ref e) => {
                if e.local_name().as_ref() == b"rPr" {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok((bold, italic, strikethrough, hyperlink))
}

/// Parse run properties from an `<a:rPr/>` Empty element. Empty
/// elements cannot carry a `<a:solidFill>` child so `color_rgb`
/// is always `None` on this path.
fn parse_run_properties_empty(
    e: &quick_xml::events::BytesStart,
    _rels: &Relationships,
) -> CoreResult<RunProps> {
    let bold = parse_bool_attr(e, b"b")?;
    let italic = parse_bool_attr(e, b"i")?;
    let strike = xml::optional_attr_str(e, b"strike")?;
    let strikethrough = strike.as_deref().is_some_and(|v| v != "noStrike");
    Ok((bold, italic, strikethrough, None))
}

/// Parse a DrawingML boolean attribute: `b="1"` → Some(true), `b="0"` → Some(false), absent → None.
fn parse_bool_attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> CoreResult<Option<bool>> {
    Ok(xml::optional_attr_str(e, key)?.map(|v| v.as_ref() != "0"))
}

/// Parse `<a:hlinkClick r:id="rId1" tooltip="..."/>` into a HyperlinkInfo.
fn parse_hlink_click(
    e: &quick_xml::events::BytesStart,
    rels: &Relationships,
) -> CoreResult<Option<HyperlinkInfo>> {
    let r_id = xml::optional_attr_str(e, b"r:id")?;
    let action = xml::optional_attr_str(e, b"action")?;

    let target = if let Some(ref r_id) = r_id {
        if let Some(rel) = rels.get_by_id(r_id) {
            if rel.target_mode == TargetMode::External {
                charge_model_text("office_pptx_text_bytes", rel.target.len())?;
                HyperlinkTarget::External(rel.target.clone())
            } else {
                charge_model_text("office_pptx_text_bytes", rel.target.len())?;
                HyperlinkTarget::Internal(rel.target.clone())
            }
        } else {
            return Err(crate::core::Error::RelationshipNotFound(r_id.to_string()));
        }
    } else if let Some(ref action) = action {
        // Internal action like ppaction://hlinksldjump
        charge_model_text("office_pptx_text_bytes", action.len())?;
        HyperlinkTarget::Internal(action.to_string())
    } else {
        return Ok(None);
    };

    Ok(Some(HyperlinkInfo { target }))
}

/// Parse `<a:fld type="..." ...>` field element.
fn parse_text_field(
    reader: &mut quick_xml::Reader<&[u8]>,
    _start: &quick_xml::events::BytesStart,
) -> CoreResult<TextField> {
    let mut text = String::new();

    loop {
        match reader.read_event()? {
            Event::Start(ref e) if e.local_name().as_ref() == b"t" => {
                text = xml::read_text_content_fast(reader)?;
                charge_model_text("office_pptx_text_bytes", text.len())?;
            }
            Event::End(ref e) if e.local_name().as_ref() == b"fld" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(TextField { text })
}

// ---------------------------------------------------------------------------
// Table parsing (DrawingML a: namespace)
// ---------------------------------------------------------------------------

/// Parse `<a:tbl>`.
fn parse_table(reader: &mut quick_xml::Reader<&[u8]>, rels: &Relationships) -> CoreResult<Table> {
    let mut rows = Vec::new();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) => match e.local_name().as_ref() {
                b"tr" => {
                    charge_model_items("office_pptx_model_items", 1)?;
                    rows.push(parse_table_row(reader, rels)?);
                }
                _ => {
                    xml::skip_element_fast(reader)?;
                }
            },
            Event::End(ref e) if e.local_name().as_ref() == b"tbl" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(Table { rows })
}

/// Parse `<a:tr>`.
fn parse_table_row(
    reader: &mut quick_xml::Reader<&[u8]>,
    rels: &Relationships,
) -> CoreResult<TableRow> {
    let mut cells = Vec::new();

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) if e.local_name().as_ref() == b"tc" => {
                charge_model_items("office_pptx_model_items", 1)?;
                cells.push(parse_table_cell(reader, e, rels)?);
            }
            Event::End(ref e) if e.local_name().as_ref() == b"tr" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(TableRow { cells })
}

/// Parse `<a:tc>`.
fn parse_table_cell(
    reader: &mut quick_xml::Reader<&[u8]>,
    start: &quick_xml::events::BytesStart,
    rels: &Relationships,
) -> CoreResult<TableCell> {
    let h_merge = xml::optional_attr_str(start, b"hMerge")?
        .is_some_and(|v| v.as_ref() == "1" || v.as_ref() == "true");
    let v_merge = xml::optional_attr_str(start, b"vMerge")?
        .is_some_and(|v| v.as_ref() == "1" || v.as_ref() == "true");

    let mut text_body = None;

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) if e.local_name().as_ref() == b"txBody" => {
                text_body = Some(parse_text_body(reader, rels)?);
            }
            Event::End(ref e) if e.local_name().as_ref() == b"tc" => {
                break;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(TableCell {
        text_body,
        h_merge,
        v_merge,
    })
}

// ---------------------------------------------------------------------------
// Notes text extraction (used by lib.rs)
// ---------------------------------------------------------------------------

/// Extract speaker notes plain text from a notes slide XML.
/// Finds the body placeholder (type="body") and extracts its text.
pub(crate) fn extract_notes_text(xml_data: &[u8]) -> CoreResult<Option<String>> {
    let rels = Relationships::empty();
    let mut reader = make_content_reader(xml_data);
    let mut shapes = Vec::new();

    // Parse the notes slide's shape tree
    loop {
        match reader.read_event()? {
            Event::Start(ref e) if e.local_name().as_ref() == b"spTree" => {
                shapes = parse_shape_tree(&mut reader, &rels)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    // Find the body placeholder and extract text
    for shape in &shapes {
        if let Shape::Auto(auto) = shape
            && let Some(ref ph) = auto.placeholder
            && ph.ph_type.as_deref() == Some("body")
            && let Some(ref tb) = auto.text_body
        {
            let text = extract_plain_text_from_body(tb);
            if !text.is_empty() {
                return Ok(Some(text));
            }
        }
    }

    Ok(None)
}

/// Extract plain text from a TextBody.
fn extract_plain_text_from_body(body: &TextBody) -> String {
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
// Tests
// ---------------------------------------------------------------------------
