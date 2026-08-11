//! Parser for PDF structure trees.
//!
//! Parses StructTreeRoot and StructElem dictionaries according to PDF spec Section 14.7.

use super::types::{StructChild, StructElem, StructTreeRoot, StructType};
use crate::document::PdfDocument;
use crate::error::Error;
use crate::object::Object;
use std::collections::{HashMap, HashSet};

use std::time::Duration;

/// Decode a PDF text string (UTF-16BE/LE with BOM, or PDFDocEncoding).
fn decode_pdf_text_string(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        // UTF-16BE with BOM
        let utf16_pairs: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&utf16_pairs)
            .unwrap_or_else(|_| String::from_utf8_lossy(bytes).to_string())
    } else if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        // UTF-16LE with BOM
        let utf16_pairs: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&utf16_pairs)
            .unwrap_or_else(|_| String::from_utf8_lossy(bytes).to_string())
    } else {
        // PDFDocEncoding
        bytes
            .iter()
            .filter_map(|&b| crate::fonts::font_dict::pdfdoc_encoding_lookup(b))
            .collect()
    }
}

/// Helper function to resolve an object (handles both direct objects and references).
fn resolve_object(document: &PdfDocument, obj: &Object) -> Result<Object, Error> {
    match obj {
        Object::Reference(obj_ref) => document.load_object(*obj_ref),
        _ => Ok(obj.clone()),
    }
}

/// Build a mapping from page object IDs to page indices.
/// This allows resolving /Pg references in marked content references.
///
/// Uses a single-pass traversal of the page tree (O(n)) instead of
/// calling get_page_ref per page (which is O(n) per call → O(n²) total).
fn build_page_map(document: &PdfDocument) -> HashMap<u32, u32> {
    let mut page_map = HashMap::new();

    // Get the root Pages node from the catalog
    let pages_ref = match document.catalog().ok().and_then(|cat| {
        cat.as_dict()
            .and_then(|d| d.get("Pages"))
            .and_then(|p| p.as_reference())
    }) {
        Some(r) => r,
        None => return page_map,
    };

    let mut index: u32 = 0;
    build_page_map_recursive(document, pages_ref, &mut page_map, &mut index);
    page_map
}

/// Recursively walk the page tree once, collecting page object IDs.
fn build_page_map_recursive(
    document: &PdfDocument,
    node_ref: crate::object::ObjectRef,
    page_map: &mut HashMap<u32, u32>,
    index: &mut u32,
) {
    let node = match document.load_object(node_ref) {
        Ok(n) => n,
        Err(_) => return,
    };
    let dict = match node.as_dict() {
        Some(d) => d,
        None => return,
    };
    let node_type = dict.get("Type").and_then(|t| t.as_name()).unwrap_or("");

    match node_type {
        "Page" => {
            page_map.insert(node_ref.id, *index);
            *index += 1;
        }
        "Pages" => {
            if let Some(kids) = dict.get("Kids").and_then(|k| k.as_array()) {
                let kid_refs: Vec<_> = kids.iter().filter_map(|k| k.as_reference()).collect();
                for kid_ref in kid_refs {
                    build_page_map_recursive(document, kid_ref, page_map, index);
                }
            }
        }
        _ => {}
    }
}

/// Parse the complete structure tree from a PDF document.
///
/// Reads the `/StructTreeRoot` from the catalog and recursively parses every
/// structure element. This is the general-purpose entry point: it applies **no**
/// time budget and **no** element cap, so it returns the whole tree however large
/// it is. Callers that need to bound the work (e.g. to keep an interactive
/// pipeline responsive and fall back to another strategy) should use
/// [`parse_structure_tree_with_budget`].
///
/// # Returns
/// * `Ok(Some(StructTreeRoot))` - the document's structure tree
/// * `Ok(None)` - the document is not tagged (no `/StructTreeRoot`)
/// * `Err(Error)` - parsing failed
pub fn parse_structure_tree(document: &PdfDocument) -> Result<Option<StructTreeRoot>, Error> {
    parse_structure_tree_inner(document, ParseBudget::unbounded())
}

/// Parse the structure tree with an optional bound on the work done.
///
/// `budget` is a wall-clock limit: `None` parses the complete tree (identical to
/// [`parse_structure_tree`]); `Some(duration)` stops once the deadline is
/// exceeded (or a companion element cap is hit) and returns `Ok(None)` so the
/// caller can fall back to another strategy. Prefer `None` unless you have a
/// concrete responsiveness requirement — a bound can drop the tree on a slow
/// machine even when the document is perfectly valid.
pub fn parse_structure_tree_with_budget(
    document: &PdfDocument,
    budget: Option<Duration>,
) -> Result<Option<StructTreeRoot>, Error> {
    parse_structure_tree_inner(document, ParseBudget::from_option(budget))
}

fn parse_structure_tree_inner(
    document: &PdfDocument,
    deadline: ParseBudget,
) -> Result<Option<StructTreeRoot>, Error> {
    let parse_start = Timer::now();

    // Get catalog
    let catalog = document.catalog()?;

    // Check for StructTreeRoot in catalog dictionary
    let catalog_dict = catalog
        .as_dict()
        .ok_or_else(|| Error::InvalidPdf("Catalog is not a dictionary".into()))?;

    let struct_tree_root_ref = match catalog_dict.get("StructTreeRoot") {
        Some(obj) => obj,
        None => return Ok(None), // Not a tagged PDF
    };

    // Build page map for resolving /Pg references
    let page_map = build_page_map(document);

    // Resolve the StructTreeRoot object
    let struct_tree_root_obj = resolve_object(document, struct_tree_root_ref)?;

    // Parse StructTreeRoot dictionary — treat non-dict (e.g. Null from a
    // corrupted parse) as "no structure tree" rather than a hard error.
    let struct_tree_dict = match struct_tree_root_obj.as_dict() {
        Some(d) => d,
        None => {
            log::warn!(
                "StructTreeRoot resolved to {} (expected dictionary), treating as no structure tree",
                struct_tree_root_obj.type_name()
            );
            return Ok(None);
        }
    };

    let mut struct_tree = StructTreeRoot::new();

    // Parse RoleMap (optional)
    if let Some(role_map_obj) = struct_tree_dict.get("RoleMap") {
        let role_map_obj = resolve_object(document, role_map_obj)?;
        if let Some(role_map_dict) = role_map_obj.as_dict() {
            for (key, value) in role_map_dict.iter() {
                if let Some(name) = value.as_name() {
                    struct_tree.role_map.insert(key.clone(), name.to_string());
                }
            }
        }
    }

    // Skip ParentTree parsing — it's expensive (recursively loads/parses objects)
    // and not needed for text extraction. The forward traversal of /K children
    // provides reading order. ParentTree is only needed for reverse lookups
    // (MCID → StructElem), which are not used in the extraction pipeline.

    // Parse K (children) - can be a single element or array of elements
    let mut element_count: usize = 0;
    let mut visited: HashSet<u32> = HashSet::new();

    if let Some(k_obj) = struct_tree_dict.get("K") {
        let k_obj = resolve_object(document, k_obj)?;

        match k_obj {
            Object::Array(arr) => {
                // Multiple root elements
                for elem_obj in arr {
                    if deadline.exceeded(element_count) {
                        log::debug!(
                            "Structure tree parse budget exceeded, falling back to content order"
                        );
                        return Ok(None);
                    }
                    // Record root element IDs before descending so that a back-reference
                    // from any descendant to this root is detectable as a cycle.
                    if let Object::Reference(obj_ref) = &elem_obj {
                        if !visited.insert(obj_ref.id) {
                            log::warn!(
                                "Cycle in structure tree: root object {} already visited, skipping",
                                obj_ref.id
                            );
                            continue;
                        }
                    }
                    if let Some(elem) = parse_struct_elem(
                        document,
                        &elem_obj,
                        &struct_tree.role_map,
                        &page_map,
                        deadline,
                        &mut element_count,
                        &mut visited,
                    )? {
                        struct_tree.add_root_element(elem);
                    }
                }
            }
            _ => {
                // Single root element
                if let Some(elem) = parse_struct_elem(
                    document,
                    &k_obj,
                    &struct_tree.role_map,
                    &page_map,
                    deadline,
                    &mut element_count,
                    &mut visited,
                )? {
                    struct_tree.add_root_element(elem);
                }
            }
        }
    }

    log::debug!(
        "Structure tree parsed: {} elements, {} root elements in {}",
        element_count,
        struct_tree.root_elements.len(),
        parse_start.elapsed_debug()
    );

    Ok(Some(struct_tree))
}

/// Parse a structure element (StructElem) from a PDF object.
///
/// Returns `Ok(None)` if the deadline is exceeded, causing the caller to
/// abandon the tree and fall back to content-stream order.
fn parse_struct_elem(
    document: &PdfDocument,
    obj: &Object,
    role_map: &HashMap<String, String>,
    page_map: &HashMap<u32, u32>,
    deadline: ParseBudget,
    element_count: &mut usize,
    visited: &mut HashSet<u32>,
) -> Result<Option<StructElem>, Error> {
    // Check the (optional) budget before doing work.
    if deadline.exceeded(*element_count) {
        return Ok(None);
    }
    *element_count += 1;

    let obj = resolve_object(document, obj)?;

    let dict = match obj.as_dict() {
        Some(d) => d,
        None => return Ok(None), // Not a dictionary, skip
    };

    // Check /Type (should be /StructElem, but optional)
    if let Some(type_obj) = dict.get("Type") {
        if let Some(type_name) = type_obj.as_name() {
            if type_name == "OBJR" {
                // Per PDF spec §14.7.4 Table 323, an OBJR (Object Reference) dictionary
                // references a StructElem (or other PDF object) via its /Obj entry.
                // Transparently dereference it so the caller gets the real StructElem.
                if let Some(obj_ref_obj) = dict.get("Obj") {
                    if let Some(obj_ref) = obj_ref_obj.as_reference() {
                        if !visited.insert(obj_ref.id) {
                            log::debug!(
                                "OBJR: cycle detected — object {} already visited, skipping",
                                obj_ref.id
                            );
                            return Ok(None);
                        }
                        match document.load_object(obj_ref) {
                            Ok(resolved) => {
                                return parse_struct_elem(
                                    document,
                                    &resolved,
                                    role_map,
                                    page_map,
                                    deadline,
                                    element_count,
                                    visited,
                                );
                            }
                            Err(e) => {
                                log::warn!(
                                    "OBJR: failed to load /Obj {} {}: {}",
                                    obj_ref.id,
                                    obj_ref.gen,
                                    e
                                );
                                return Ok(None);
                            }
                        }
                    }
                }
                return Ok(None);
            }
            if type_name != "StructElem" {
                return Ok(None); // Not a StructElem
            }
        }
    }

    // Get /S (structure type) - REQUIRED
    let s_obj = match dict.get("S") {
        Some(obj) => obj,
        None => return Ok(None), // Missing /S, skip gracefully
    };
    let s_name = match s_obj.as_name() {
        Some(name) => name,
        None => return Ok(None), // /S not a name, skip
    };

    // Map custom types to standard types using RoleMap
    // Preserve the original role name when mapping occurs
    let mapped = role_map.get(s_name);
    let struct_type_str = mapped.map(|s| s.as_str()).unwrap_or(s_name);
    let struct_type = StructType::from_str(struct_type_str);

    let mut struct_elem = StructElem::new(struct_type);
    if mapped.is_some() {
        struct_elem.source_role = Some(s_name.to_string());
    }

    // Get /Pg (page) - optional, resolve to page number
    if let Some(Object::Reference(pg_ref)) = dict.get("Pg") {
        if let Some(&page_num) = page_map.get(&pg_ref.id) {
            struct_elem.page = Some(page_num);
        }
    }

    // Skip /A (attributes) during text extraction — not needed for reading order.
    // Skip /Alt (alternate description) — not needed for text extraction.

    // Get /ActualText (replacement text) - optional, per PDF spec Section 14.9.4
    // When present, this text replaces all descendant content for the element.
    if let Some(at_obj) = dict.get("ActualText") {
        let at_obj = resolve_object(document, at_obj)?;
        if let Some(at_bytes) = at_obj.as_string() {
            let text = decode_pdf_text_string(at_bytes);
            if !text.is_empty() {
                struct_elem.actual_text = Some(text);
            }
        }
    }

    // Parse /K (children)
    if let Some(k_obj_raw) = dict.get("K") {
        // When /K is an indirect reference that resolves to a struct elem dictionary
        // (as opposed to an array), we lose the object ID after resolve_object and
        // the Dictionary arm of parse_k_children cannot check for cycles.
        // Load it here while we still have the reference ID, insert into visited,
        // and short-circuit if it has already been visited.
        if let Object::Reference(r) = k_obj_raw {
            let k_resolved = match document.load_object(*r) {
                Ok(obj) => obj,
                Err(e) => {
                    log::warn!("Failed to load /K reference {}: {}", r.id, e);
                    return Ok(Some(struct_elem));
                }
            };
            if k_resolved.as_dict().is_some() {
                // /K points directly at a struct elem — guard against cycles.
                if !visited.insert(r.id) {
                    log::warn!(
                        "Cycle in structure tree: /K object {} already visited, skipping children",
                        r.id
                    );
                    return Ok(Some(struct_elem));
                }
            }
            parse_k_children(
                document,
                &k_resolved,
                &mut struct_elem,
                role_map,
                page_map,
                deadline,
                element_count,
                visited,
            )?;
        } else {
            let k_obj = resolve_object(document, k_obj_raw)?;
            parse_k_children(
                document,
                &k_obj,
                &mut struct_elem,
                role_map,
                page_map,
                deadline,
                element_count,
                visited,
            )?;
        }
    }

    Ok(Some(struct_elem))
}

/// Parse the /K entry (children) of a structure element.
fn parse_k_children(
    document: &PdfDocument,
    k_obj: &Object,
    parent: &mut StructElem,
    role_map: &HashMap<String, String>,
    page_map: &HashMap<u32, u32>,
    deadline: ParseBudget,
    element_count: &mut usize,
    visited: &mut HashSet<u32>,
) -> Result<(), Error> {
    match k_obj {
        Object::Integer(mcid) => {
            // Single MCID — bare integer child references the page's
            // own content stream (ISO 32000-1:2008 §14.7.5.4.2 "MCR"
            // form is reserved for cross-stream refs).
            let page = parent.page.unwrap_or(0);
            parent.add_child(StructChild::MarkedContentRef {
                mcid: *mcid as u32,
                page,
                scope: crate::structure::McidScope::Page(page),
            });
        }

        Object::Array(arr) => {
            // Array of children
            for child_obj in arr {
                // Check the (optional) budget before descending.
                if deadline.exceeded(*element_count) {
                    return Ok(());
                }

                // Guard against cycles before resolving: an indirect reference that has
                // already been visited would resolve to a dictionary and slip through to
                // parse_struct_elem without any ID to check. Capture the ID now, while we
                // still have the unresolved Reference, so the check happens before loading.
                if let Object::Reference(obj_ref) = child_obj {
                    if !visited.insert(obj_ref.id) {
                        log::warn!(
                            "Cycle in structure tree: object {} already visited, skipping",
                            obj_ref.id
                        );
                        continue;
                    }
                }

                let child_obj = resolve_object(document, child_obj)?;

                match &child_obj {
                    Object::Integer(mcid) => {
                        // Bare integer MCID — page's own content stream.
                        let page = parent.page.unwrap_or(0);
                        parent.add_child(StructChild::MarkedContentRef {
                            mcid: *mcid as u32,
                            page,
                            scope: crate::structure::McidScope::Page(page),
                        });
                    }

                    Object::Dictionary(_) => {
                        // Could be a StructElem or marked content reference
                        if let Some(child_elem) = parse_struct_elem(
                            document,
                            &child_obj,
                            role_map,
                            page_map,
                            deadline,
                            element_count,
                            visited,
                        )? {
                            parent.add_child(StructChild::StructElem(Box::new(child_elem)));
                        } else {
                            // Try parsing as marked content reference
                            if let Some(mcr) =
                                parse_marked_content_ref(document, &child_obj, page_map)?
                            {
                                parent.add_child(mcr);
                            }
                        }
                    }

                    Object::Reference(obj_ref) => {
                        // Double-indirect reference — guard against cycles here too.
                        if !visited.insert(obj_ref.id) {
                            log::warn!(
                                "Cycle in structure tree: object {} already visited, skipping",
                                obj_ref.id
                            );
                            continue;
                        }
                        // Resolve indirect reference and try to parse as StructElem
                        match document.load_object(*obj_ref) {
                            Ok(resolved) => {
                                if let Some(child_elem) = parse_struct_elem(
                                    document,
                                    &resolved,
                                    role_map,
                                    page_map,
                                    deadline,
                                    element_count,
                                    visited,
                                )? {
                                    parent.add_child(StructChild::StructElem(Box::new(child_elem)));
                                } else if let Some(mcr) =
                                    parse_marked_content_ref(document, &resolved, page_map)?
                                {
                                    parent.add_child(mcr);
                                }
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to resolve ObjectRef {} {}: {}",
                                    obj_ref.id,
                                    obj_ref.gen,
                                    e
                                );
                            }
                        }
                    }

                    _ => {
                        // Unknown child type, skip
                    }
                }
            }
        }

        Object::Dictionary(_) => {
            // Single dictionary child
            if let Some(child_elem) = parse_struct_elem(
                document,
                k_obj,
                role_map,
                page_map,
                deadline,
                element_count,
                visited,
            )? {
                parent.add_child(StructChild::StructElem(Box::new(child_elem)));
            } else {
                // Try parsing as marked content reference
                if let Some(mcr) = parse_marked_content_ref(document, k_obj, page_map)? {
                    parent.add_child(mcr);
                }
            }
        }

        Object::Reference(obj_ref) => {
            // Guard against cycles: skip if this object has already been visited.
            if !visited.insert(obj_ref.id) {
                log::warn!(
                    "Cycle in structure tree: object {} already visited, skipping",
                    obj_ref.id
                );
                return Ok(());
            }
            // Resolve indirect reference and try to parse as StructElem
            match document.load_object(*obj_ref) {
                Ok(resolved) => {
                    if let Some(child_elem) = parse_struct_elem(
                        document,
                        &resolved,
                        role_map,
                        page_map,
                        deadline,
                        element_count,
                        visited,
                    )? {
                        parent.add_child(StructChild::StructElem(Box::new(child_elem)));
                    } else if let Some(mcr) =
                        parse_marked_content_ref(document, &resolved, page_map)?
                    {
                        parent.add_child(mcr);
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Failed to resolve ObjectRef {} {}: {}",
                        obj_ref.id,
                        obj_ref.gen,
                        e
                    );
                }
            }
        }

        _ => {
            // Unknown K type
        }
    }

    Ok(())
}

/// Parse a marked content reference dictionary.
///
/// According to ISO 32000-1:2008 §14.7.5.4.2, a marked content reference
/// (MCR) dictionary has:
/// - `/Type /MCR` (optional but, when present, fixes the dict's identity)
/// - `/Pg` — page reference (optional; inherits enclosing StructElem
///   `/Pg` when absent)
/// - `/MCID` — required marked-content identifier
/// - `/Stm` — optional reference to the content stream that holds the
///   MCID. When absent, the MCID lives in the page's own content
///   stream. When present, the stream's dictionary determines the
///   scope:
///     - `/Subtype /Form` → Form XObject scope
///     - `/PatternType 1` → Tiling Pattern scope
///     - anything else → falls back to Page scope with a debug log,
///       since we can't classify the stream.
fn parse_marked_content_ref(
    document: &PdfDocument,
    obj: &Object,
    page_map: &HashMap<u32, u32>,
) -> Result<Option<StructChild>, Error> {
    let dict = match obj.as_dict() {
        Some(d) => d,
        None => return Ok(None),
    };

    // Check for /Type /MCR
    if let Some(type_obj) = dict.get("Type") {
        if let Some(type_name) = type_obj.as_name() {
            if type_name != "MCR" {
                return Ok(None);
            }
        }
    }

    // Get /MCID
    let mcid = match dict.get("MCID").and_then(|obj| obj.as_integer()) {
        Some(mcid) => mcid,
        None => return Ok(None), // Missing /MCID, skip gracefully
    };

    // Get /Pg (page reference) and resolve to page number.
    let page = dict
        .get("Pg")
        .and_then(|pg_obj| {
            if let Object::Reference(pg_ref) = pg_obj {
                page_map.get(&pg_ref.id).copied()
            } else {
                None
            }
        })
        .unwrap_or(0); // Default to page 0 if no /Pg

    // Resolve /Stm to determine the MCID's content-stream scope.
    // `/StmOwn` is the "inline image" form (§14.7.5.4.2) — out of
    // scope here since inline images do not carry their own MCID
    // namespace.
    let scope = resolve_mcr_scope(document, dict, page);

    Ok(Some(StructChild::MarkedContentRef {
        mcid: mcid as u32,
        page,
        scope,
    }))
}

/// Determine the `McidScope` for a marked-content reference dict.
///
/// When `/Stm` is absent the MCID belongs to the enclosing page's
/// content stream; the scope is `Page(page)`. When `/Stm` is present
/// the referenced stream's dictionary classifies the scope: Form
/// XObjects produce `Form(ref)`, Tiling Patterns produce
/// `Pattern(ref)`. An unclassifiable `/Stm` (unresolved, missing
/// subtype, or unrecognised type) falls back to `Page(page)` with a
/// debug log so the assembler still has *some* lookup key — at worst
/// the lookup misses and the raw glyphs flow through unchanged, which
/// is strictly safer than a collision.
fn resolve_mcr_scope(
    document: &PdfDocument,
    mcr_dict: &HashMap<String, Object>,
    page: u32,
) -> crate::structure::McidScope {
    use crate::structure::McidScope;

    let stm = match mcr_dict.get("Stm") {
        Some(s) => s,
        None => return McidScope::Page(page),
    };

    let stm_ref = match stm {
        Object::Reference(r) => *r,
        _ => {
            log::debug!("MCR /Stm is not an indirect reference; defaulting to page scope");
            return McidScope::Page(page);
        }
    };

    let stm_obj = match document.load_object(stm_ref) {
        Ok(o) => o,
        Err(e) => {
            log::debug!(
                "MCR /Stm {} {} could not be resolved ({}); defaulting to page scope",
                stm_ref.id,
                stm_ref.gen,
                e
            );
            return McidScope::Page(page);
        }
    };

    // For both streams and dictionaries, locate the dict-half.
    let stream_dict: &HashMap<String, Object> = match &stm_obj {
        Object::Stream { dict, .. } => dict,
        Object::Dictionary(d) => d,
        _ => {
            log::debug!(
                "MCR /Stm {} {} resolves to non-stream/non-dict; defaulting to page scope",
                stm_ref.id,
                stm_ref.gen
            );
            return McidScope::Page(page);
        }
    };

    // Form XObject: /Type /XObject /Subtype /Form (or /Subtype /Form
    // alone — producers sometimes omit /Type because it is optional on
    // an XObject stream per §8.8).
    if let Some(subtype) = stream_dict.get("Subtype").and_then(|o| o.as_name()) {
        if subtype == "Form" {
            return McidScope::Form(stm_ref);
        }
    }

    // Tiling Pattern: /PatternType 1 (per §8.7.3.3). Shading patterns
    // (PatternType 2) do not have a content stream of their own and
    // cannot host MCIDs.
    if let Some(pt) = stream_dict.get("PatternType").and_then(|o| o.as_integer()) {
        if pt == 1 {
            return McidScope::Pattern(stm_ref);
        }
    }

    log::debug!(
        "MCR /Stm {} {} has unknown subtype/pattern type; defaulting to page scope",
        stm_ref.id,
        stm_ref.gen
    );
    McidScope::Page(page)
}

mod budget;

use budget::{ParseBudget, Timer, MAX_STRUCT_ELEMENTS};

#[cfg(test)]
mod tests;
