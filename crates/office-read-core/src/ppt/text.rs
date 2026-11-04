//! Text extraction from PPT binary records.

use super::persist::{self, PersistDirectory};
use super::records::*;
use super::{PptError, Result};

/// Guards against pathologically deep (or maliciously crafted) shape nesting.
const MAX_SHAPE_DEPTH: usize = 64;

/// Text type from TextHeaderAtom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextType {
    /// Title placeholder text.
    Title,
    /// Body / content placeholder text.
    Body,
    /// Speaker notes text.
    Notes,
    /// Other or unclassified text.
    Other,
    /// Centered body placeholder.
    CenterBody,
    /// Centered title placeholder.
    CenterTitle,
    /// Half-size body placeholder.
    HalfBody,
    /// Quarter-size body placeholder.
    QuarterBody,
}

impl TextType {
    /// Convert a `TextHeaderAtom` type integer to a `TextType`.
    pub fn from_u32(val: u32) -> Self {
        match val {
            0 => Self::Title,
            1 => Self::Body,
            2 => Self::Notes,
            3 => Self::Other,
            4 => Self::CenterBody,
            5 => Self::CenterTitle,
            6 => Self::HalfBody,
            7 => Self::QuarterBody,
            _ => Self::Other,
        }
    }
}

/// A text run extracted from a slide.
#[derive(Debug, Clone)]
pub struct TextRun {
    /// The role of this text within its slide.
    pub text_type: TextType,
    /// The decoded text content.
    pub text: String,
}

/// Extract per-slide text from a "PowerPoint Document" stream.
///
/// `current_user` is the raw "Current User" stream, if present.
///
/// PPT97 files are saved incrementally: a slide's *current* content lives in
/// a `Slide` container located via the persist object directory, not
/// necessarily wherever a naive front-to-back scan first stumbles on
/// slide-shaped data (stale, superseded copies of records are routinely left
/// behind in the stream by earlier saves). This resolves each slide through
/// that directory — see the `persist` module — falling back to weaker
/// heuristics only when a usable directory can't be built at all (e.g. a
/// minimal hand-built stream that never went through a real save cycle).
pub fn extract_slides_text(stream: &[u8], current_user: Option<&[u8]>) -> Result<Vec<SlideText>> {
    if let Some(dir) = persist::build(stream, current_user)?
        && let Some(slides) = extract_slides_via_persist(stream, &dir)?
    {
        // A resolved-but-entirely-textless result is ambiguous: it's the
        // correct answer for a genuinely text-free deck (image-only
        // slides), but it's also what a corrupted persist chain that
        // resolved to the wrong offsets looks like. Fall through to the
        // weaker heuristics below rather than committing to it — if they
        // also come up empty, this was the right answer all along.
        if !slides.is_empty() && slides.iter().any(|s| !s.text_runs.is_empty()) {
            return attach_persisted_notes(stream, &dir, slides);
        }
    }

    if let Some(slide_list) = find_descendant(stream, RT_SLIDE_LIST_WITH_TEXT, SLWT_SLIDES, 0)? {
        let slides = extract_slides_from_slide_list_cache(slide_list)?;
        if !slides.is_empty() {
            return attach_notes(stream, slides);
        }
    }

    // Last resort: no resolvable structure at all — dump whatever text atoms
    // exist anywhere in the stream as a single slide.
    let mut runs = Vec::new();
    extract_shape_text(stream, 0, &[], &mut runs)?;
    if runs.is_empty() {
        Ok(Vec::new())
    } else {
        charge_slide()?;
        Ok(vec![SlideText { text_runs: runs }])
    }
}

fn attach_persisted_notes(
    stream: &[u8],
    directory: &PersistDirectory,
    mut slides: Vec<SlideText>,
) -> Result<Vec<SlideText>> {
    let Some(notes) = extract_text_list_via_persist(stream, directory, SLWT_NOTES, RT_NOTES)?
    else {
        return attach_notes(stream, slides);
    };
    for (slide, note) in slides.iter_mut().zip(notes) {
        slide.text_runs.extend(
            note.text_runs
                .into_iter()
                .filter(|run| !run.text.trim().is_empty())
                .map(|mut run| {
                    run.text_type = TextType::Notes;
                    run
                }),
        );
    }
    Ok(slides)
}

fn attach_notes(stream: &[u8], mut slides: Vec<SlideText>) -> Result<Vec<SlideText>> {
    let mut notes = match find_descendant(stream, RT_SLIDE_LIST_WITH_TEXT, SLWT_NOTES, 0)? {
        Some(notes_list) => extract_slides_from_slide_list_cache(notes_list)?,
        None => Vec::new(),
    };
    if notes.is_empty() {
        let mut containers = Vec::new();
        collect_containers(stream, RT_NOTES, 0, &mut containers)?;
        for container in containers {
            let mut text_runs = Vec::new();
            extract_shape_text(container, 0, &[], &mut text_runs)?;
            if !text_runs.is_empty() {
                charge_slide()?;
                notes.push(SlideText { text_runs });
            }
        }
    }
    if notes.is_empty() {
        let known = slides
            .iter()
            .flat_map(|slide| slide.text_runs.iter().map(|run| run.text.as_str()))
            .collect::<std::collections::HashSet<_>>();
        let mut text_runs = Vec::new();
        extract_shape_text(stream, 0, &[], &mut text_runs)?;
        text_runs.retain(|run| !run.text.trim().is_empty() && !known.contains(run.text.as_str()));
        if !text_runs.is_empty() {
            charge_slide()?;
            notes.push(SlideText { text_runs });
        }
    }
    for (slide, note) in slides.iter_mut().zip(notes) {
        slide.text_runs.extend(
            note.text_runs
                .into_iter()
                .filter(|run| !run.text.trim().is_empty())
                .map(|mut run| {
                    run.text_type = TextType::Notes;
                    run
                }),
        );
    }
    Ok(slides)
}

fn collect_containers<'a>(
    data: &'a [u8],
    record_type: u16,
    depth: usize,
    output: &mut Vec<&'a [u8]>,
) -> Result<()> {
    if depth > MAX_SHAPE_DEPTH {
        return Err(PptError::ResourceLimit {
            resource: "office_ppt_shape_depth",
            limit: MAX_SHAPE_DEPTH as u64,
            observed: depth as u64,
        });
    }
    for record in RecordIter::new(data) {
        let record = record?;
        if record.header.is_container() {
            if record.header.rec_type == record_type {
                output.push(record.data);
            }
            collect_containers(record.data, record_type, depth + 1, output)?;
        }
    }
    Ok(())
}

/// Resolve the current "Slides" list through the persist directory and
/// extract each slide's shape text from its resolved `Slide` container.
///
/// Also collects each slide's own outline-text sequence — the
/// `TextHeaderAtom`/`TextCharsAtom`/`TextBytesAtom` runs that directly follow
/// its `SlidePersistAtom` in `SlideListWithTextContainer` — because many
/// placeholder shapes don't embed their text directly at all: they hold only
/// an `OutlineTextRefAtom`, an index into that same per-slide sequence
/// ([MS-PPT] 2.4.15.6). Resolving text purely from the `Slide` container's own
/// records, without this table, silently drops that text.
///
/// Returns `None` if the `DocumentContainer` or its slide list can't be
/// resolved at all (directory present but unusable); returns `Some(vec![])`
/// if the slide list resolves but is empty.
fn extract_slides_via_persist(
    stream: &[u8],
    dir: &PersistDirectory,
) -> Result<Option<Vec<SlideText>>> {
    extract_text_list_via_persist(stream, dir, SLWT_SLIDES, RT_SLIDE)
}

fn extract_text_list_via_persist(
    stream: &[u8],
    dir: &PersistDirectory,
    list_instance: u16,
    container_type: u16,
) -> Result<Option<Vec<SlideText>>> {
    let Some(doc_offset) = dir.resolve(dir.doc_persist_id) else {
        return Ok(None);
    };
    let Some(doc_children) = bounded_container_children(stream, doc_offset, RT_DOCUMENT)? else {
        return Ok(None);
    };
    let Some(slide_list) = find_child(doc_children, RT_SLIDE_LIST_WITH_TEXT, list_instance)? else {
        return Ok(None);
    };

    let mut slides = Vec::new();
    let mut current_persist_id: Option<u32> = None;
    let mut outline_texts: Vec<TextRun> = Vec::new();
    let mut current_type = TextType::Other;

    for rec in RecordIter::new(slide_list) {
        let rec = rec?;
        match rec.header.rec_type {
            RT_SLIDE_PERSIST_ATOM if rec.data.len() >= 4 => {
                if let Some(persist_id_ref) = current_persist_id.take() {
                    slides.push(resolve_persisted_text(
                        stream,
                        dir,
                        persist_id_ref,
                        container_type,
                        &outline_texts,
                    )?);
                    charge_slide()?;
                }
                current_persist_id = Some(u32::from_le_bytes([
                    rec.data[0],
                    rec.data[1],
                    rec.data[2],
                    rec.data[3],
                ]));
                outline_texts.clear();
                current_type = TextType::Other;
            }
            RT_TEXT_HEADER if rec.data.len() >= 4 => {
                let t = u32::from_le_bytes([rec.data[0], rec.data[1], rec.data[2], rec.data[3]]);
                current_type = TextType::from_u32(t);
            }
            RT_TEXT_CHARS => {
                // Positional index into this list is meaningful (it's what
                // OutlineTextRefAtom references) — an empty run still
                // occupies a slot and must not be skipped here.
                push_text_run(&mut outline_texts, current_type, decode_utf16le(rec.data)?)?;
            }
            RT_TEXT_BYTES => {
                push_text_run(&mut outline_texts, current_type, decode_bytes(rec.data)?)?;
            }
            _ => {}
        }
    }
    if let Some(persist_id_ref) = current_persist_id.take() {
        slides.push(resolve_persisted_text(
            stream,
            dir,
            persist_id_ref,
            container_type,
            &outline_texts,
        )?);
        charge_slide()?;
    }

    Ok(Some(slides))
}

/// Resolve one slide's shape text: locate its `Slide` container via the
/// persist directory and walk its shape tree, resolving any
/// `OutlineTextRefAtom` references against `outline_texts`.
///
/// Every persist-directory-resolved slide is kept regardless of whether text
/// was found — an image-only slide is still a slide, and the presentation's
/// true slide count matters for numbering.
fn resolve_persisted_text(
    stream: &[u8],
    dir: &PersistDirectory,
    persist_id_ref: u32,
    container_type: u16,
    outline_texts: &[TextRun],
) -> Result<SlideText> {
    let mut text_runs = Vec::new();
    if let Some(offset) = dir.resolve(persist_id_ref)
        && let Some(children) = bounded_container_children(stream, offset, container_type)?
    {
        extract_shape_text(children, 0, outline_texts, &mut text_runs)?;
    }
    Ok(SlideText { text_runs })
}

/// Recursively collect a shape tree's text, in document order, from a bounded
/// record region (a resolved `Slide`/`Notes`/`MainMaster` container's own
/// children).
///
/// Text comes from two places: `TextHeaderAtom` + `TextCharsAtom`/
/// `TextBytesAtom` pairs embedded directly in a shape, or an
/// `OutlineTextRefAtom` resolved against `outline_texts` (see
/// [`extract_slides_via_persist`]) for shapes that store their text there
/// instead. Pass `&[]` when no outline-text table applies (e.g. the
/// no-persist-directory fallback).
///
/// Bounded per [`RecordIter`]'s container semantics: a corrupted/oversized
/// length on one shape can, at worst, truncate the remaining shapes within
/// that *same* container — it can never affect anything outside the region
/// this function was called with.
fn extract_shape_text(
    data: &[u8],
    depth: usize,
    outline_texts: &[TextRun],
    out: &mut Vec<TextRun>,
) -> Result<()> {
    if depth > MAX_SHAPE_DEPTH {
        return Err(PptError::ResourceLimit {
            resource: "office_ppt_shape_depth",
            limit: MAX_SHAPE_DEPTH as u64,
            observed: depth as u64,
        });
    }
    let mut current_type = TextType::Other;

    for rec in RecordIter::new(data) {
        let rec = rec?;
        match rec.header.rec_type {
            RT_TEXT_HEADER if rec.data.len() >= 4 => {
                let t = u32::from_le_bytes([rec.data[0], rec.data[1], rec.data[2], rec.data[3]]);
                current_type = TextType::from_u32(t);
            }
            RT_TEXT_CHARS => {
                let text = decode_utf16le(rec.data)?;
                if !text.is_empty() {
                    push_text_run(out, current_type, text)?;
                }
            }
            RT_TEXT_BYTES => {
                let text = decode_bytes(rec.data)?;
                if !text.is_empty() {
                    push_text_run(out, current_type, text)?;
                }
            }
            RT_OUTLINE_TEXT_REF_ATOM if rec.data.len() >= 4 => {
                let index =
                    i32::from_le_bytes([rec.data[0], rec.data[1], rec.data[2], rec.data[3]]);
                if index >= 0
                    && let Some(run) = outline_texts.get(index as usize)
                    && !run.text.is_empty()
                {
                    push_text_run(out, run.text_type, run.text.clone())?;
                }
            }
            _ if rec.header.is_container() => {
                extract_shape_text(rec.data, depth + 1, outline_texts, out)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Fallback used only when the persist directory can't be resolved at all:
/// derive slide boundaries from a `SlideListWithText`'s own inline text cache
/// — a `SlidePersistAtom` followed directly by `TextHeaderAtom` +
/// `TextCharsAtom`/`TextBytesAtom` pairs ([MS-PPT] 2.4.14.3). The resolved
/// `Slide` container (primary path above) additionally resolves
/// `OutlineTextRefAtom` indirection against this same sequence; this fallback
/// is only reached when persist resolution isn't available at all.
fn extract_slides_from_slide_list_cache(slide_list: &[u8]) -> Result<Vec<SlideText>> {
    let mut slides: Vec<SlideText> = Vec::new();
    let mut current: Option<SlideText> = None;
    let mut current_type = TextType::Other;

    for rec in RecordIter::new(slide_list) {
        let rec = rec?;
        match rec.header.rec_type {
            RT_SLIDE_PERSIST_ATOM => {
                if let Some(slide) = current.take()
                    && !slide.text_runs.is_empty()
                {
                    slides.push(slide);
                    charge_slide()?;
                }
                current = Some(SlideText {
                    text_runs: Vec::new(),
                });
                current_type = TextType::Other;
            }
            RT_TEXT_HEADER if rec.data.len() >= 4 => {
                let t = u32::from_le_bytes([rec.data[0], rec.data[1], rec.data[2], rec.data[3]]);
                current_type = TextType::from_u32(t);
            }
            RT_TEXT_CHARS => {
                if let Some(slide) = current.as_mut() {
                    let text = decode_utf16le(rec.data)?;
                    if !text.is_empty() {
                        push_text_run(&mut slide.text_runs, current_type, text)?;
                    }
                }
            }
            RT_TEXT_BYTES => {
                if let Some(slide) = current.as_mut() {
                    let text = decode_bytes(rec.data)?;
                    if !text.is_empty() {
                        push_text_run(&mut slide.text_runs, current_type, text)?;
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(slide) = current
        && !slide.text_runs.is_empty()
    {
        slides.push(slide);
        charge_slide()?;
    }

    Ok(slides)
}

/// Bounded children of the container at `offset`, if it is one of type `rec_type`.
fn bounded_container_children(
    stream: &[u8],
    offset: usize,
    rec_type: u16,
) -> Result<Option<&[u8]>> {
    let Some(header_end) = offset.checked_add(8) else {
        return Err(PptError::InvalidRecord("record offset overflow".to_owned()));
    };
    let Some(header_bytes) = stream.get(offset..header_end) else {
        return Ok(None);
    };
    let header = RecordHeader::parse(header_bytes)?;
    if header.rec_type != rec_type || !header.is_container() {
        return Ok(None);
    }
    let start = header_end;
    let end = start
        .checked_add(header.rec_len as usize)
        .ok_or_else(|| PptError::InvalidRecord("record length overflow".to_owned()))?;
    let children = stream
        .get(start..end)
        .ok_or_else(|| PptError::InvalidRecord("truncated container".to_owned()))?;
    Ok(Some(children))
}

/// Single-level search for a direct child record matching `rec_type` and
/// `instance`, returning its bounded children.
fn find_child(data: &[u8], rec_type: u16, instance: u16) -> Result<Option<&[u8]>> {
    for rec in RecordIter::new(data) {
        let rec = rec?;
        if rec.header.rec_type == rec_type && rec.header.rec_instance == instance {
            return Ok(Some(rec.data));
        }
    }
    Ok(None)
}

/// Bounded recursive search for a descendant record matching `rec_type` and
/// `instance`, returning its bounded children. Only used by the legacy
/// fallback path, where the structure isn't guaranteed to place the target at
/// any particular depth.
fn find_descendant(
    data: &[u8],
    rec_type: u16,
    instance: u16,
    depth: usize,
) -> Result<Option<&[u8]>> {
    if depth > MAX_SHAPE_DEPTH {
        return Err(PptError::ResourceLimit {
            resource: "office_ppt_shape_depth",
            limit: MAX_SHAPE_DEPTH as u64,
            observed: depth as u64,
        });
    }
    for rec in RecordIter::new(data) {
        let rec = rec?;
        if rec.header.rec_type == rec_type && rec.header.rec_instance == instance {
            return Ok(Some(rec.data));
        }
        if rec.header.is_container()
            && let Some(found) = find_descendant(rec.data, rec_type, instance, depth + 1)?
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Text content of a single slide.
#[derive(Debug, Clone)]
pub struct SlideText {
    /// All text runs belonging to this slide.
    pub text_runs: Vec<TextRun>,
}

fn decode_utf16le(data: &[u8]) -> Result<String> {
    let worst_case = data.len().saturating_div(2).saturating_mul(3);
    crate::budget::check_ppt_text_allocation(worst_case).map_err(map_budget_error)?;
    let chars: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&chars))
}

fn decode_bytes(data: &[u8]) -> Result<String> {
    crate::budget::check_ppt_text_allocation(data.len()).map_err(map_budget_error)?;
    Ok(data.iter().map(|&byte| byte as char).collect())
}

fn push_text_run(out: &mut Vec<TextRun>, text_type: TextType, text: String) -> Result<()> {
    crate::budget::charge_ppt_item("office_ppt_text_runs").map_err(map_budget_error)?;
    crate::budget::charge_ppt_text(text.len()).map_err(map_budget_error)?;
    out.push(TextRun { text_type, text });
    Ok(())
}

fn charge_slide() -> Result<()> {
    crate::budget::charge_ppt_item("office_ppt_slides").map_err(map_budget_error)
}

fn map_budget_error(error: crate::OfficeReadError) -> PptError {
    match error {
        crate::OfficeReadError::ResourceLimit {
            resource,
            limit,
            observed,
        } => PptError::ResourceLimit {
            resource,
            limit,
            observed,
        },
        crate::OfficeReadError::Cancelled => PptError::Cancelled,
        crate::OfficeReadError::Io(error) => PptError::Io(error),
        crate::OfficeReadError::Invalid { .. } | crate::OfficeReadError::Unsupported { .. } => {
            PptError::Corrupted("resource validation failed".to_owned())
        }
    }
}
