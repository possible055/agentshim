use super::*;

/// Scan raw file bytes for candidate ObjStm positions.
///
/// Each hit is `(object_number, byte_offset_of_N_G_obj_header)`. We look
/// for the shape `N G obj ... /Type /ObjStm` within a small window after
/// each object header so that the caller can then `load_uncompressed_object`
/// at exactly that offset without parsing the whole file body.
///
/// The scan is intentionally tolerant: it doesn't require `/Type`
/// `/ObjStm` to be separated by whitespace (many producers write
/// `/Type/ObjStm`), doesn't anchor on any particular position within the
/// header, and doesn't rely on xref entries being correct — which is the
/// whole point of the recovery path it serves.
/// Window scanned after the xref table when looking for the `trailer` keyword and its
/// dictionary. Generous for a structure that is normally a few hundred bytes.
pub(super) const TRAILER_SCAN_BYTES: usize = 1024 * 1024;

/// Longest lookahead `find_objstm_candidates` performs from a header.
pub(super) const DICT_PEEK_BYTES: usize = 2048;
/// Window overlap that keeps that lookahead intact across a boundary.
pub(super) const OBJSTM_SCAN_OVERLAP_BYTES: usize = DICT_PEEK_BYTES + 64;

pub(super) fn find_objstm_candidates(
    content: &[u8],
    chunk: &crate::xref_reconstruction::ScanChunk,
) -> Vec<(u32, u64)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < content.len() {
        let valid_start = pos == 0
            || content[pos - 1] == b'\n'
            || content[pos - 1] == b'\r'
            || content[pos - 1] == b' ';
        if !valid_start || !content[pos].is_ascii_digit() {
            pos += 1;
            continue;
        }
        let header_start = pos;

        // Parse N (object number)
        let num_start = pos;
        while pos < content.len() && content[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos >= content.len() || content[pos] != b' ' {
            pos = header_start + 1;
            continue;
        }
        let obj_num: u32 = match std::str::from_utf8(&content[num_start..pos])
            .ok()
            .and_then(|s| s.parse().ok())
        {
            Some(n) => n,
            None => {
                pos = header_start + 1;
                continue;
            }
        };
        pos += 1;

        // Parse G (generation)
        let gen_start = pos;
        while pos < content.len() && content[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos >= content.len() || content[pos] != b' ' {
            pos = header_start + 1;
            continue;
        }
        if std::str::from_utf8(&content[gen_start..pos])
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .is_none()
        {
            pos = header_start + 1;
            continue;
        }
        pos += 1;

        // Require literal "obj"
        if pos + 3 > content.len() || &content[pos..pos + 3] != b"obj" {
            pos = header_start + 1;
            continue;
        }

        // Peek up to DICT_PEEK_BYTES ahead for `/Type` followed (after
        // optional whitespace) by `/ObjStm`. We don't decompress — the
        // ObjStm dict header is always uncompressed plaintext even when
        // the stream body is Flate-encoded.
        let window_end = (pos + DICT_PEEK_BYTES).min(content.len());
        let window = &content[pos..window_end];
        if contains_objstm_marker(window) {
            if let Some(absolute) = chunk.absolute(header_start) {
                out.push((obj_num, absolute as u64));
            }
        }

        pos = header_start + 1;
    }
    out
}

pub(super) fn contains_objstm_marker(window: &[u8]) -> bool {
    // Tolerant match: find `/Type` then allow optional whitespace before `/ObjStm`.
    let mut i = 0;
    while i + 5 <= window.len() {
        if &window[i..i + 5] == b"/Type" {
            let mut j = i + 5;
            while j < window.len()
                && (window[j] == b' '
                    || window[j] == b'\t'
                    || window[j] == b'\r'
                    || window[j] == b'\n')
            {
                j += 1;
            }
            if j + 7 <= window.len() && &window[j..j + 7] == b"/ObjStm" {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Append ink names declared by `Separation` and `DeviceN` colour spaces
/// in `cs_dict` to `out`. Reserved colorants `/All` and `/None` (§8.6.6.4)
/// are skipped. Caller is responsible for deduping across multiple calls.
///
/// When `doc` is `Some`, indirect references inside each colour-space array
/// (e.g. a DeviceN whose names list is `4 0 R` rather than inline) are
/// resolved. Tools that hand-build inline arrays and don't need indirection
/// resolution can pass `None`.
///
/// Used by both [`PdfDocument::get_page_inks`] and
/// [`PdfDocument::get_page_inks_deep`] so the per-colorant rules live in
/// exactly one place.
pub(super) fn extract_inks_from_color_space_dict(
    cs_dict: &std::collections::HashMap<String, Object>,
    doc: Option<&PdfDocument>,
    out: &mut Vec<String>,
) {
    let mut visited: std::collections::HashSet<ObjectRef> = std::collections::HashSet::new();
    for cs_def in cs_dict.values() {
        collect_inks_from_color_space(cs_def, doc, out, &mut visited, 0);
    }
}

/// Inner walker — surfaces inks from a single colour-space definition.
/// Factored out of [`extract_inks_from_color_space_dict`] so the
/// Pattern arm can recurse into its underlying colour space without
/// requiring a synthetic single-entry dict.
///
/// **Cycle handling:** the Pattern arm recurses into the underlying
/// colour space (§8.7.3.1). A self-referential array such as
/// `5 0 obj [/Pattern 5 0 R]` would otherwise blow the stack, so
/// indirect references are de-duplicated via `visited` (keyed on
/// `ObjectRef`) and total depth is capped at `MAX_RECURSION_DEPTH`
/// — the same backstop used by [`PdfDocument::walk_form_xobject_tree_for_inks`].
pub(super) fn collect_inks_from_color_space(
    cs_def: &Object,
    doc: Option<&PdfDocument>,
    out: &mut Vec<String>,
    visited: &mut std::collections::HashSet<ObjectRef>,
    depth: u32,
) {
    if depth >= MAX_RECURSION_DEPTH {
        return;
    }
    let deref = |obj: &Object| -> Object {
        match (obj.as_reference(), doc) {
            (Some(r), Some(d)) => d.load_object(r).unwrap_or_else(|_| obj.clone()),
            _ => obj.clone(),
        }
    };

    let arr = match cs_def.as_array() {
        Some(a) => a,
        None => return,
    };
    if arr.len() < 2 {
        return;
    }
    let cs_type = match arr.first().and_then(Object::as_name) {
        Some(n) => n,
        None => return,
    };
    match cs_type {
        "Pattern" => {
            // ISO 32000-1 §8.7.3.1: a Pattern colour space's
            // optional second array element is the underlying
            // colour space (uncoloured Tiling carries the
            // underlying space's tints). Recurse so a Pattern
            // with /Separation or /DeviceN underlying surfaces
            // the spot colorants for plate allocation.
            //
            // Guard against self-referential cycles (e.g.
            // `5 0 obj [/Pattern 5 0 R]`): an indirect underlying
            // ref is recorded in `visited`; a repeat hit terminates
            // the recursion silently.
            if let Some(r) = arr[1].as_reference() {
                if !visited.insert(r) {
                    return;
                }
            }
            let underlying = deref(&arr[1]);
            collect_inks_from_color_space(&underlying, doc, out, visited, depth + 1);
        }
        "Separation" => {
            // §8.6.6.2: [/Separation /InkName /AlternateCS /TintTransform].
            // The name slot is usually inline but resolve indirects for safety.
            let name_obj = deref(&arr[1]);
            if let Some(ink) = name_obj.as_name() {
                if ink != "All" && ink != "None" {
                    out.push(ink.to_string());
                }
            }
        }
        "DeviceN" => {
            // §8.6.6.5: [/DeviceN <names-array> /AlternateCS /TintTransform <attrs>].
            // The names array is commonly emitted as an indirect reference
            // when the same colorant set is shared across multiple DeviceN
            // spaces; resolve before unpacking the names.
            let names_obj = match arr.get(1) {
                Some(o) => deref(o),
                None => return,
            };
            // ISO 32000-1 §8.6.6.5 / Table 73: the optional 5th array
            // element is the attributes dictionary. When its `/Process`
            // sub-dictionary declares a `/Components` array, those names
            // are PROCESS colorants (riding the page's process plates),
            // not spot inks. The same rule applies whether the attrs
            // dict's `/Subtype` is `/DeviceN` (the default, PDF 1.6) or
            // `/NChannel` (PDF 1.7 stricter subtype) — §8.6.6.5 names the
            // /Process key on both subtypes. Build the process-name set
            // here so the colorants loop can filter against it.
            let process_names: std::collections::HashSet<String> = arr
                .get(4)
                .map(&deref)
                .as_ref()
                .and_then(Object::as_dict)
                .and_then(|attrs| attrs.get("Process"))
                .map(&deref)
                .as_ref()
                .and_then(Object::as_dict)
                .and_then(|proc_dict| proc_dict.get("Components"))
                .map(&deref)
                .as_ref()
                .and_then(Object::as_array)
                .map(|comps| {
                    comps
                        .iter()
                        .filter_map(|o| o.as_name().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(inks) = names_obj.as_array() {
                for ink_obj in inks {
                    if let Some(ink) = ink_obj.as_name() {
                        if ink != "All" && ink != "None" && !process_names.contains(ink) {
                            out.push(ink.to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Per-page MCID action computed from the
/// [`crate::structure::ActualTextIndex`].
///
/// Drives every consumer of struct-tree-scope `/ActualText`
/// (`extract_text`'s structure-order assembler, the raw-span applier,
/// and the ordered-span applier). The map is computed once per page
/// from the cached `ActualTextIndex` plus the visibility / MC-scope
/// filters; consumers then dispatch per MCID without re-walking the
/// structure tree.
#[derive(Debug, Clone)]
pub(crate) enum ActualTextAction {
    /// Replace this MCID's span text with the supplied string AND drop
    /// subsequent spans / MCIDs in the same consecutive-replacement
    /// run. Assigned to exactly one MCID per emitting run: the first
    /// visible MCID that is not exempted by MC-scope-wins.
    EmitAndSuppress(std::sync::Arc<str>),
    /// Suppress the raw glyphs for this MCID without emitting anything.
    /// Used for run continuations after the run's emission MCID, for
    /// suppress-only entries (non-first-page coverage of a multi-page
    /// ActualText scope), and for MCIDs in a fully-hidden run.
    Suppress,
}
