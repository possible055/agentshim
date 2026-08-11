//! Structure tree traversal for extracting reading order.
//!
//! Implements pre-order traversal of structure trees to determine correct reading order.

use super::types::{
    ActualTextIndex, McidScope, StructChild, StructElem, StructTreeRoot, StructType,
};
use crate::error::Error;
use std::sync::Arc;

/// Role this content plays inside a List (PDF spec §14.8.4.3).
///
/// MCRs nested under list-context ancestors carry their role so the
/// markdown converter can emit `- item` / `1. item` correctly even when
/// the immediate parent of the MCR is a Span or P (the common Word /
/// Acrobat output shape `LI → LBody → Span → MCR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRole {
    /// Inside an LI (list item) but not under Lbl/LBody yet (or LI
    /// itself holds the MCR directly).
    LI,
    /// Inside the Lbl (label) sub-element of an LI — the bullet/number.
    Lbl,
    /// Inside the LBody (body) sub-element of an LI — the item text.
    LBody,
}

/// Represents an ordered content item extracted from structure tree.
#[derive(Debug, Clone)]
pub struct OrderedContent {
    /// Page number
    pub page: u32,

    /// Marked Content ID (None for word break markers)
    pub mcid: Option<u32>,

    /// Structure type (for semantic information)
    pub struct_type: String,

    /// Pre-parsed structure type for efficient access
    pub parsed_type: StructType,

    /// Is this a heading?
    ///
    /// True when the MCR is nested under any heading ancestor (H, H1..H6),
    /// not just when the immediate parent is a heading. Word-generated
    /// tagged PDFs commonly wrap heading text in `H1 → Span → MCR`, where
    /// the heading semantic must still be recovered.
    pub is_heading: bool,

    /// If the MCR is nested under any heading ancestor, the level of that
    /// ancestor (H1 → 1, …, H6 → 6, generic H → 1). None otherwise.
    pub heading_level: Option<u8>,

    /// Role inside a list, when nested under any L/LI ancestor. None if
    /// this MCR has no list ancestor.
    pub list_role: Option<ListRole>,

    /// Is this a block-level element?
    pub is_block: bool,

    /// Is this a word break marker (WB element)?
    ///
    /// When true, a space should be inserted at this position during
    /// text assembly. This supports CJK text that uses WB elements
    /// to mark word boundaries.
    pub is_word_break: bool,

    /// Identifier of the nearest block-level ancestor (P, H*, LI, Sect,
    /// Div, Art, …) — increments each time the traversal enters a new
    /// block element. Two MCRs that share a `block_id` belong to the
    /// same logical paragraph; a change in `block_id` between adjacent
    /// MCRs is the structure-tree-authoritative paragraph boundary
    /// (PDF spec ISO 32000-1:2008 §14.8.4). The markdown / HTML
    /// converters rely on this to split paragraphs when a tagged PDF's
    /// inter-paragraph gap is too small for the geometric heuristic.
    /// 0 means "no enclosing block element seen" (root-level Span).
    pub block_id: u32,

    /// True when this MCR is nested under a table grouping element
    /// (Table / THead / TBody / TFoot / TR / TH / TD). The plain-text
    /// assembler separates consecutive table rows with a single newline
    /// rather than the geometric multi-line gap.
    pub in_table: bool,

    /// True when this MCR is nested under a `Code` element. Preformatted —
    /// its line breaks are significant and converters must not reflow them.
    pub preformatted: bool,

    /// Identifier of the nearest `Sect` / `Art` / `Part` grouping-element
    /// ancestor (ISO 32000-1:2008 §14.8.4.2), or `None` at the document level.
    /// Two MCRs that share a `section_id` belong to the same logical section —
    /// the spec-authoritative, page-independent grouping that
    /// `extract_structured` surfaces as a per-region section index, so chapters
    /// stay grouped across pages without geometric guessing (#734 §5/§6).
    pub section_id: Option<u32>,

    /// Actual text replacement from /ActualText (optional)
    /// Per PDF spec Section 14.9.4, when present this replaces all
    /// descendant content with the specified text.
    pub actual_text: Option<String>,

    /// Content-stream scope of the MCID (ISO 32000-1:2008 §14.7.4.3).
    ///
    /// `McidScope::Page(page)` for MCIDs drawn directly by the page's
    /// content stream (the dominant case). `Form(_)` / `Pattern(_)`
    /// when the structure tree's MCR carried a `/Stm` reference into
    /// a Form XObject or Tiling Pattern, so the ActualText applier can
    /// look up `(scope, mcid)` without colliding with same-mcid keys
    /// in other namespaces. None when this `OrderedContent` is a word-
    /// break marker (no MCID).
    pub mcid_scope: Option<McidScope>,
}

/// Inheritable context propagated down the structure tree during traversal.
///
/// Tracks the nearest heading and list ancestors so deeply nested MCRs
/// (`H1 → Span → MCR`, `LI → LBody → Span → MCR`) carry the correct
/// semantic role on the resulting `OrderedContent`. Without this, the
/// markdown converter saw the immediate parent (Span / P) and lost the
/// heading / list-item information altogether.
#[derive(Debug, Clone, Copy, Default)]
struct InheritedContext {
    heading_level: Option<u8>,
    list_role: Option<ListRole>,
    /// Identifier of the nearest block-level ancestor — see
    /// `OrderedContent::block_id`.
    block_id: u32,
    /// Identifier of the nearest `Sect`/`Art`/`Part` ancestor — see
    /// `OrderedContent::section_id`.
    section_id: Option<u32>,
    /// True when the MCR is nested under a table grouping element
    /// (Table / THead / TBody / TFoot / TR / TH / TD). Used by the
    /// plain-text assembler to separate table rows with a single newline
    /// instead of the geometric multi-line gap (ISO 32000-1 §14.8.4.3.4:
    /// table rows are stacked block-level rows, not free-leading paragraphs).
    in_table: bool,
    /// True when the MCR is nested under a `Code` element — preformatted
    /// content whose line breaks are significant. The converters must NOT
    /// reflow such lines into a single paragraph.
    preformatted: bool,
}

impl InheritedContext {
    /// Returns true when `t` is a block-level element that should bump
    /// the paragraph counter on entry. Spans, links, and similar inline
    /// elements do not.
    fn is_paragraph_block(t: &StructType) -> bool {
        matches!(
            t,
            StructType::P
                | StructType::H
                | StructType::H1
                | StructType::H2
                | StructType::H3
                | StructType::H4
                | StructType::H5
                | StructType::H6
                | StructType::LI
                | StructType::Lbl
                | StructType::LBody
                | StructType::Sect
                | StructType::Div
                | StructType::Art
                | StructType::Part
                | StructType::Note
                | StructType::Reference
                | StructType::BibEntry
                | StructType::Code
                | StructType::TR
                | StructType::TH
                | StructType::TD
        )
    }

    fn descend(self, child: &StructType, counter: &mut u32) -> Self {
        let heading_level = match child {
            StructType::H1 => Some(1),
            StructType::H2 => Some(2),
            StructType::H3 => Some(3),
            StructType::H4 => Some(4),
            StructType::H5 => Some(5),
            StructType::H6 => Some(6),
            // Generic /H carries no level on its own.
            StructType::H => Some(self.heading_level.unwrap_or(1)),
            _ => self.heading_level,
        };
        let list_role = match child {
            StructType::Lbl => Some(ListRole::Lbl),
            StructType::LBody => Some(ListRole::LBody),
            StructType::LI => Some(self.list_role.unwrap_or(ListRole::LI)),
            // L starts list context but doesn't itself hold MCRs as items;
            // its LI children promote to ListRole::LI on descent.
            StructType::L => self.list_role,
            _ => self.list_role,
        };
        let block_id = if Self::is_paragraph_block(child) {
            *counter += 1;
            *counter
        } else {
            self.block_id
        };
        // A Sect/Art/Part opens a new logical section (§14.8.4.2); its own
        // block_id (just bumped above) becomes the section id its descendants
        // inherit. Other elements keep the enclosing section.
        let section_id = match child {
            StructType::Sect | StructType::Art | StructType::Part => Some(block_id),
            _ => self.section_id,
        };
        let in_table = self.in_table
            || matches!(
                child,
                StructType::Table
                    | StructType::THead
                    | StructType::TBody
                    | StructType::TFoot
                    | StructType::TR
                    | StructType::TH
                    | StructType::TD
            );
        let preformatted = self.preformatted || matches!(child, StructType::Code);
        Self {
            heading_level,
            list_role,
            block_id,
            section_id,
            in_table,
            preformatted,
        }
    }
}

/// Traverse the structure tree and extract ordered content for a specific page.
///
/// This performs a pre-order traversal of the structure tree, extracting
/// marked content references in document order.
///
/// # Arguments
/// * `struct_tree` - The structure tree root
/// * `page_num` - The page number to extract content for
///
/// # Returns
/// * Vector of ordered content items for the specified page
pub fn traverse_structure_tree(
    struct_tree: &StructTreeRoot,
    page_num: u32,
) -> Result<Vec<OrderedContent>, Error> {
    let mut result = Vec::new();
    let mut block_counter = 0u32;

    // Traverse each root element
    for root_elem in &struct_tree.root_elements {
        traverse_element(
            root_elem,
            page_num,
            InheritedContext::default(),
            &mut block_counter,
            &mut result,
        )?;
    }

    Ok(result)
}

/// Traverse the structure tree once and build content for ALL pages.
///
/// This is much more efficient than calling `traverse_structure_tree` once per page,
/// which would walk the entire tree N times. Instead, we walk the tree once and
/// collect content items into per-page buckets.
///
/// Returns a HashMap mapping page numbers to their ordered content items.
pub fn traverse_structure_tree_all_pages(
    struct_tree: &StructTreeRoot,
) -> std::collections::HashMap<u32, Vec<OrderedContent>> {
    let mut result: std::collections::HashMap<u32, Vec<OrderedContent>> =
        std::collections::HashMap::new();

    let mut block_counter = 0u32;
    for root_elem in &struct_tree.root_elements {
        traverse_element_all_pages(
            root_elem,
            InheritedContext::default(),
            &mut block_counter,
            &mut result,
        );
    }

    result
}

/// Recursively traverse a structure element, collecting content for all pages.
///
/// `ctx` carries inherited semantics from heading and list ancestors so deeply
/// nested MCRs (e.g. `H1 → Span → MCR`, `LI → LBody → Span → MCR`) emit
/// content tagged with the right role, not just the immediate parent's role.
fn traverse_element_all_pages(
    elem: &StructElem,
    ctx: InheritedContext,
    block_counter: &mut u32,
    result: &mut std::collections::HashMap<u32, Vec<OrderedContent>>,
) {
    let struct_type_str = format!("{:?}", elem.struct_type);
    let parsed_type = elem.struct_type.clone();
    let descended = ctx.descend(&parsed_type, block_counter);
    let is_heading_inherited = descended.heading_level.is_some();
    let is_block = elem.struct_type.is_block();
    let is_word_break = elem.struct_type.is_word_break();

    // /ActualText is resolved separately via `build_actualtext_index`
    // — assemblers consult the index to position the replacement and to
    // suppress descendant MCIDs (per ISO 32000-1:2008 §14.9.4 the
    // replacement covers the entire subtree, but emitting it has to
    // respect the multi-page emit-once rule which a per-page traversal
    // cannot enforce). The traversal therefore continues to record
    // descendant MCIDs so the structure-order MCID list stays complete;
    // the assembler drops the suppressed ones at emit time.

    // Process children in order
    for child in &elem.children {
        match child {
            StructChild::MarkedContentRef {
                mcid,
                page,
                scope: mcid_scope,
            } => {
                result.entry(*page).or_default().push(OrderedContent {
                    page: *page,
                    mcid: Some(*mcid),
                    struct_type: struct_type_str.clone(),
                    parsed_type: parsed_type.clone(),
                    is_heading: is_heading_inherited,
                    heading_level: descended.heading_level,
                    list_role: descended.list_role,
                    is_block,
                    is_word_break: false,
                    block_id: descended.block_id,
                    section_id: descended.section_id,
                    in_table: descended.in_table,
                    preformatted: descended.preformatted,
                    actual_text: None,
                    mcid_scope: Some(mcid_scope.clone()),
                });
            }

            StructChild::StructElem(child_elem) => {
                // If parent is WB, emit word break markers before processing child
                if is_word_break {
                    let child_pages = collect_pages(child_elem);
                    for page in child_pages {
                        result.entry(page).or_default().push(OrderedContent {
                            page,
                            mcid: None,
                            struct_type: struct_type_str.clone(),
                            parsed_type: parsed_type.clone(),
                            is_heading: false,
                            heading_level: None,
                            list_role: descended.list_role,
                            is_block: false,
                            is_word_break: true,
                            block_id: descended.block_id,
                            section_id: descended.section_id,
                            in_table: descended.in_table,
                            preformatted: descended.preformatted,
                            actual_text: None,
                            mcid_scope: None,
                        });
                    }
                }
                traverse_element_all_pages(child_elem, descended, block_counter, result);
            }

            StructChild::ObjectRef(_obj_num, _gen) => {
                log::debug!("Skipping unresolved ObjectRef({}, {})", _obj_num, _gen);
            }
        }
    }
}

/// Collect all page numbers that a structure element has content on.
fn collect_pages(elem: &StructElem) -> Vec<u32> {
    let mut pages = Vec::new();
    collect_pages_recursive(elem, &mut pages);
    pages.sort_unstable();
    pages.dedup();
    pages
}

fn collect_pages_recursive(elem: &StructElem, pages: &mut Vec<u32>) {
    if let Some(page) = elem.page {
        pages.push(page);
    }
    for child in &elem.children {
        match child {
            StructChild::MarkedContentRef { page, .. } => {
                pages.push(*page);
            }
            StructChild::StructElem(child_elem) => {
                collect_pages_recursive(child_elem, pages);
            }
            _ => {}
        }
    }
}

/// Recursively traverse a structure element.
///
/// Performs pre-order traversal:
/// 1. Process current element's marked content (if on target page)
/// 2. Recursively process children in order
/// 3. Handle WB (word break) elements by emitting markers
fn traverse_element(
    elem: &StructElem,
    target_page: u32,
    ctx: InheritedContext,
    block_counter: &mut u32,
    result: &mut Vec<OrderedContent>,
) -> Result<(), Error> {
    let struct_type_str = format!("{:?}", elem.struct_type);
    let parsed_type = elem.struct_type.clone();
    let descended = ctx.descend(&parsed_type, block_counter);
    let is_heading_inherited = descended.heading_level.is_some();
    let is_block = elem.struct_type.is_block();
    let is_word_break = elem.struct_type.is_word_break();

    // /ActualText is resolved separately via `build_actualtext_index`;
    // see `traverse_element_all_pages` for the rationale.

    // If this is a WB (word break) element, emit a word break marker
    if is_word_break {
        result.push(OrderedContent {
            page: target_page,
            mcid: None,
            struct_type: struct_type_str.clone(),
            parsed_type: parsed_type.clone(),
            is_heading: false,
            heading_level: None,
            list_role: descended.list_role,
            is_block: false,
            is_word_break: true,
            block_id: descended.block_id,
            section_id: descended.section_id,
            in_table: descended.in_table,
            preformatted: descended.preformatted,
            actual_text: None,
            mcid_scope: None,
        });
        // WB elements typically have no children, but process any just in case
    }

    // Process children in order
    for child in &elem.children {
        match child {
            StructChild::MarkedContentRef {
                mcid,
                page,
                scope: mcid_scope,
            } => {
                // If this marked content is on the target page, add it
                if *page == target_page {
                    result.push(OrderedContent {
                        page: *page,
                        mcid: Some(*mcid),
                        struct_type: struct_type_str.clone(),
                        parsed_type: parsed_type.clone(),
                        is_heading: is_heading_inherited,
                        heading_level: descended.heading_level,
                        list_role: descended.list_role,
                        is_block,
                        is_word_break: false,
                        block_id: descended.block_id,
                        section_id: descended.section_id,
                        in_table: descended.in_table,
                        preformatted: descended.preformatted,
                        actual_text: None,
                        mcid_scope: Some(mcid_scope.clone()),
                    });
                }
            }

            StructChild::StructElem(child_elem) => {
                // Recursively traverse child element
                traverse_element(child_elem, target_page, descended, block_counter, result)?;
            }

            StructChild::ObjectRef(_obj_num, _gen) => {
                // ObjectRef should be resolved at parse time (structure/parser.rs).
                // If we encounter one here, it means the reference couldn't be resolved.
                log::debug!("Skipping unresolved ObjectRef({}, {})", _obj_num, _gen);
            }
        }
    }

    Ok(())
}

/// Check if a structure element has any content on the target page.
///
/// Used only by tests since per-element ActualText gating moved into
/// the [`ActualTextIndex`] (which records per-emission `first_page`).
#[cfg(test)]
fn has_content_on_page(elem: &StructElem, target_page: u32) -> bool {
    if elem.page == Some(target_page) {
        return true;
    }
    for child in &elem.children {
        match child {
            StructChild::MarkedContentRef { page, .. } => {
                if *page == target_page {
                    return true;
                }
            }
            StructChild::StructElem(child_elem) => {
                if has_content_on_page(child_elem, target_page) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Build an [`ActualTextIndex`] resolving every structure-tree
/// `/ActualText` declaration in a single pre-order traversal.
///
/// Per ISO 32000-1:2008 §14.9.4, a structure element may carry an
/// `/ActualText` entry that replaces all of its descendant content for
/// text-extraction purposes. The replacement scope is the bearing
/// element's subtree. When ActualText scopes nest, the inner
/// replacement wins for the `(page, mcid)` pairs the inner element
/// covers.
///
/// The returned index lets every extraction surface apply ActualText
/// consistently:
///   - `covered_mcids` lists `(page, mcid)` pairs whose raw glyph spans
///     must be suppressed.
///   - `mcid_to_actual_text` resolves the innermost replacement for
///     each covered `(page, mcid)` whose pair sits on the bearing
///     element's first page (or the inner scope's first page when
///     nested) — consumers iterate per-page MCIDs in structure-tree
///     order and emit `mcid_to_actual_text[(page, mcid)]` whenever the
///     value changes across consecutive covered MCIDs, giving
///     correct one-emission-per-replacement output.
///   - `suppress_only` is the rest: `(page, mcid)` pairs on a non-first
///     page of a multi-page subtree, where the replacement has already
///     fired on the bearing element's first page; the raw glyphs are
///     still suppressed but no second emission is produced.
///
/// Empty `/ActualText` strings and elements with no descendant MCID
/// contribute nothing.
pub fn build_actualtext_index(struct_tree: &StructTreeRoot) -> ActualTextIndex {
    let mut idx = ActualTextIndex::new();
    for root in &struct_tree.root_elements {
        walk_actualtext(root, None, &mut idx);
    }
    idx
}

/// One ActualText scope, threaded down the traversal so descendant
/// `(scope, mcid)` pairs know which scope to attribute them to.
#[derive(Clone)]
struct ActiveScope {
    /// Innermost active replacement text.
    text: Arc<str>,
    /// First page (in pre-order) on which a Page-scoped descendant
    /// MCID of this ActualText scope appears. The emit-once-across-
    /// pages rule applies *only* to Page-scoped descendants: a
    /// multi-page subtree emits once on `first_page` and `suppress_only`
    /// covers the rest. Form- and Pattern-scoped descendants live in
    /// their own per-stream namespace (ISO 32000-1:2008 §14.7.4.3); each
    /// one emits at its own anchor.
    ///
    /// `None` when the subtree has no Page-scoped MCR descendant — in
    /// which case the suppress-only fallback is irrelevant.
    first_page: Option<u32>,
}

/// Pre-order walker for [`build_actualtext_index`].
///
/// `inherited` carries the innermost active scope from our ancestors.
/// For each element bearing `/ActualText` we pre-scan our own subtree
/// to find the first Page-scoped page (so the across-pages emit-once
/// rule still works), then walk children with our scope active.
fn walk_actualtext(elem: &StructElem, inherited: Option<ActiveScope>, idx: &mut ActualTextIndex) {
    let own_text: Option<Arc<str>> = elem
        .actual_text
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(Arc::from);

    let active = if let Some(text) = own_text {
        // Pre-scan to find this scope's first Page-scoped descendant.
        // Subtrees with *only* Form/Pattern descendants get
        // `first_page = None` — the per-stream namespaces don't share
        // an emit-once rule (there is no "first page" for a Form
        // XObject's content stream from the structure tree's
        // perspective).
        //
        // When the subtree has no descendant MCR of any kind, drop
        // the scope: nothing to attach to.
        if has_any_mcr(elem) {
            Some(ActiveScope {
                text,
                first_page: first_page_in_subtree(elem),
            })
        } else {
            None
        }
    } else {
        None
    };

    // The active scope for our subtree: our own if any, else inherited.
    // Inner-wins: when our own scope exists, we override inherited for
    // every descendant.
    let scope = active.clone().or(inherited.clone());

    for child in &elem.children {
        match child {
            StructChild::MarkedContentRef {
                mcid,
                page,
                scope: mcid_scope,
            } => {
                if let Some(ref s) = scope {
                    let key = (mcid_scope.clone(), *mcid);
                    idx.covered_mcids.insert(key.clone());

                    // Emit-once rule:
                    // - Page-scoped: emit on the first page seen, suppress
                    //   on the others (cross-page subtrees).
                    // - Form/Pattern-scoped: emit on every covered key;
                    //   each form/pattern is its own namespace and the
                    //   StructElem covers one such stream at most for
                    //   each contained MCID.
                    let should_emit = match mcid_scope {
                        crate::structure::McidScope::Page(_) => s.first_page == Some(*page),
                        crate::structure::McidScope::Form(_)
                        | crate::structure::McidScope::Pattern(_) => true,
                    };

                    if should_emit {
                        idx.mcid_to_actual_text.insert(key, s.text.clone());
                    } else {
                        // Non-first-page coverage for a multi-page Page
                        // subtree: suppress raw glyphs but do not
                        // re-emit; the replacement already fired on
                        // `s.first_page`.
                        idx.suppress_only.insert(key);
                    }
                }
            }
            StructChild::StructElem(child_elem) => {
                walk_actualtext(child_elem, scope.clone(), idx);
            }
            StructChild::ObjectRef(_, _) => {
                // Unresolved external reference — consistent with the
                // rest of the traversal, we skip.
            }
        }
    }
}

/// Find the first Page-scoped page (in pre-order) on which any
/// descendant MCR inside `elem`'s subtree sits. `None` when no
/// descendant is Page-scoped (the subtree may still have Form- or
/// Pattern-scoped descendants).
fn first_page_in_subtree(elem: &StructElem) -> Option<u32> {
    for child in &elem.children {
        match child {
            StructChild::MarkedContentRef { page, scope, .. } => {
                if matches!(scope, crate::structure::McidScope::Page(_)) {
                    return Some(*page);
                }
            }
            StructChild::StructElem(c) => {
                if let Some(p) = first_page_in_subtree(c) {
                    return Some(p);
                }
            }
            StructChild::ObjectRef(_, _) => {}
        }
    }
    None
}

/// Returns true when `elem`'s subtree contains at least one
/// `MarkedContentRef` of any scope.
fn has_any_mcr(elem: &StructElem) -> bool {
    for child in &elem.children {
        match child {
            StructChild::MarkedContentRef { .. } => return true,
            StructChild::StructElem(c) => {
                if has_any_mcr(c) {
                    return true;
                }
            }
            StructChild::ObjectRef(_, _) => {}
        }
    }
    false
}

/// Extract all marked content IDs in reading order for a page.
///
/// This is a simpler interface that just returns the MCIDs in order,
/// which can be used to reorder extracted text blocks.
///
/// Note: Word break (WB) markers are filtered out since they don't have MCIDs.
/// Use `traverse_structure_tree` directly if you need word break information.
///
/// # Arguments
/// * `struct_tree` - The structure tree root
/// * `page_num` - The page number
///
/// # Returns
/// * Vector of MCIDs in reading order
pub fn extract_reading_order(
    struct_tree: &StructTreeRoot,
    page_num: u32,
) -> Result<Vec<u32>, Error> {
    let ordered_content = traverse_structure_tree(struct_tree, page_num)?;
    Ok(ordered_content
        .into_iter()
        .filter_map(|c| c.mcid) // Filter out word break markers (mcid=None)
        .collect())
}

#[cfg(test)]
mod tests;
