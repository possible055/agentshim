use super::*;

#[test]
fn test_simple_traversal() {
    // Create a simple structure tree:
    // Document
    //   ├─ P (MCID=0, page=0)
    //   └─ P (MCID=1, page=0)
    let mut root = StructElem::new(StructType::Document);

    let mut p1 = StructElem::new(StructType::P);
    p1.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let mut p2 = StructElem::new(StructType::P);
    p2.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    root.add_child(StructChild::StructElem(Box::new(p1)));
    root.add_child(StructChild::StructElem(Box::new(p2)));

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    // Extract reading order
    let order = extract_reading_order(&struct_tree, 0).unwrap();
    assert_eq!(order, vec![0, 1]);
}

#[test]
fn test_page_filtering() {
    // Create structure with content on different pages
    let mut root = StructElem::new(StructType::Document);

    let mut p1 = StructElem::new(StructType::P);
    p1.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let mut p2 = StructElem::new(StructType::P);
    p2.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 1,
        scope: crate::structure::McidScope::Page(1),
    });

    root.add_child(StructChild::StructElem(Box::new(p1)));
    root.add_child(StructChild::StructElem(Box::new(p2)));

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    // Extract page 0 - should only get MCID 0
    let order_page_0 = extract_reading_order(&struct_tree, 0).unwrap();
    assert_eq!(order_page_0, vec![0]);

    // Extract page 1 - should only get MCID 1
    let order_page_1 = extract_reading_order(&struct_tree, 1).unwrap();
    assert_eq!(order_page_1, vec![1]);
}

#[test]
fn test_nested_structure() {
    // Create nested structure:
    // Document
    //   └─ Sect
    //       ├─ H1 (MCID=0)
    //       └─ P (MCID=1)
    let mut root = StructElem::new(StructType::Document);

    let mut sect = StructElem::new(StructType::Sect);

    let mut h1 = StructElem::new(StructType::H1);
    h1.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let mut p = StructElem::new(StructType::P);
    p.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    sect.add_child(StructChild::StructElem(Box::new(h1)));
    sect.add_child(StructChild::StructElem(Box::new(p)));

    root.add_child(StructChild::StructElem(Box::new(sect)));

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    // Should traverse in order: H1 (MCID 0), then P (MCID 1)
    let order = extract_reading_order(&struct_tree, 0).unwrap();
    assert_eq!(order, vec![0, 1]);
}

#[test]
fn test_word_break_elements() {
    // Create structure with WB (word break) elements for CJK text:
    // P
    //   ├─ Span (MCID=0) - "你好"
    //   ├─ WB             - word boundary marker
    //   └─ Span (MCID=1) - "世界"
    let mut root = StructElem::new(StructType::P);

    let mut span1 = StructElem::new(StructType::Span);
    span1.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let wb = StructElem::new(StructType::WB);

    let mut span2 = StructElem::new(StructType::Span);
    span2.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    root.add_child(StructChild::StructElem(Box::new(span1)));
    root.add_child(StructChild::StructElem(Box::new(wb)));
    root.add_child(StructChild::StructElem(Box::new(span2)));

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    // traverse_structure_tree should include the word break marker
    let ordered = traverse_structure_tree(&struct_tree, 0).unwrap();
    assert_eq!(ordered.len(), 3); // MCID 0, WB, MCID 1
    assert_eq!(ordered[0].mcid, Some(0));
    assert!(!ordered[0].is_word_break);
    assert_eq!(ordered[1].mcid, None); // WB has no MCID
    assert!(ordered[1].is_word_break);
    assert_eq!(ordered[2].mcid, Some(1));
    assert!(!ordered[2].is_word_break);

    // extract_reading_order should filter out WB markers
    let mcids = extract_reading_order(&struct_tree, 0).unwrap();
    assert_eq!(mcids, vec![0, 1]); // Only MCIDs, no WB
}

#[test]
fn test_empty_tree() {
    let struct_tree = StructTreeRoot::new();
    let order = extract_reading_order(&struct_tree, 0).unwrap();
    assert!(order.is_empty());
}

#[test]
fn test_empty_page() {
    let mut root = StructElem::new(StructType::Document);
    let mut p = StructElem::new(StructType::P);
    p.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    root.add_child(StructChild::StructElem(Box::new(p)));

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    // Page 5 has no content
    let order = extract_reading_order(&struct_tree, 5).unwrap();
    assert!(order.is_empty());
}

#[test]
fn test_nested_heading_propagates_is_heading_to_inner_mcr() {
    // Word365 / docling pattern: H1 wraps Span which holds the actual MCR.
    // The MCR must inherit is_heading from its H1 ancestor, not from
    // the immediate Span parent (Span.is_heading() == false).
    // Reproduces issue #377 word365_structure regression.
    let mut h1 = StructElem::new(StructType::H1);
    let mut span = StructElem::new(StructType::Span);
    span.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    h1.add_child(StructChild::StructElem(Box::new(span)));

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(h1);

    let ordered = traverse_structure_tree(&struct_tree, 0).unwrap();
    let heading_mcrs: Vec<_> = ordered.iter().filter(|c| c.is_heading).collect();
    assert_eq!(
        heading_mcrs.len(),
        1,
        "H1 → Span → MCR must propagate is_heading=true to the inner MCR"
    );
    assert_eq!(heading_mcrs[0].mcid, Some(0));
    // Same expectation from the all-pages traversal used by markdown.
    let by_page = traverse_structure_tree_all_pages(&struct_tree);
    let heading_mcrs_all: Vec<_> = by_page
        .get(&0)
        .unwrap()
        .iter()
        .filter(|c| c.is_heading)
        .collect();
    assert_eq!(heading_mcrs_all.len(), 1);
}

#[test]
fn test_nested_li_lbody_keeps_list_context() {
    // word365 / pdfa pattern: LI → LBody → MCR. LBody is the list-item
    // body and must be tagged as such; LI ancestry must be discoverable
    // when emitting markdown bullets.
    let mut li = StructElem::new(StructType::LI);
    let mut lbody = StructElem::new(StructType::LBody);
    lbody.add_child(StructChild::MarkedContentRef {
        mcid: 7,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    li.add_child(StructChild::StructElem(Box::new(lbody)));
    let mut l = StructElem::new(StructType::L);
    l.add_child(StructChild::StructElem(Box::new(li)));

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(l);

    let ordered = traverse_structure_tree(&struct_tree, 0).unwrap();
    let li_mcrs: Vec<_> = ordered
        .iter()
        .filter(|c| matches!(c.list_role, Some(crate::structure::ListRole::LBody)))
        .collect();
    assert_eq!(
        li_mcrs.len(),
        1,
        "LI → LBody → MCR must carry list_role=LBody on the inner MCR"
    );
}

/// D8b coverage — every standard heading level (H1..H6) propagates
/// to a deeply nested MCR. Parametrised over all six levels in the
/// same test to keep the lock-in compact.
#[test]
fn test_nested_heading_propagates_for_h1_through_h6() {
    let levels = [
        (StructType::H1, 1u8),
        (StructType::H2, 2),
        (StructType::H3, 3),
        (StructType::H4, 4),
        (StructType::H5, 5),
        (StructType::H6, 6),
    ];
    for (h_type, expected_level) in levels {
        // H? → Sect → Span → MCR (3-level nesting, reflects the
        // worst-case shape seen in word365_structure-class fixtures).
        let mut head = StructElem::new(h_type.clone());
        let mut sect = StructElem::new(StructType::Sect);
        let mut span = StructElem::new(StructType::Span);
        span.add_child(StructChild::MarkedContentRef {
            mcid: 42,
            page: 0,
            scope: crate::structure::McidScope::Page(0),
        });
        sect.add_child(StructChild::StructElem(Box::new(span)));
        head.add_child(StructChild::StructElem(Box::new(sect)));
        let mut tree = StructTreeRoot::new();
        tree.add_root_element(head);

        let ordered = traverse_structure_tree(&tree, 0).unwrap();
        let item = ordered.iter().find(|c| c.mcid == Some(42)).unwrap();
        assert!(
            item.is_heading,
            "H{} → Sect → Span → MCR must carry is_heading=true",
            expected_level
        );
        assert_eq!(
            item.heading_level,
            Some(expected_level),
            "H{} ancestor must propagate heading_level={}",
            expected_level,
            expected_level
        );
    }
}

/// D8b coverage — generic /H without an explicit level reports
/// heading_level=Some(1) (the only sensible default per spec
/// §14.8.4.2 when no surrounding heading exists).
#[test]
fn test_generic_h_without_level_defaults_to_h1() {
    let mut h = StructElem::new(StructType::H);
    let mut span = StructElem::new(StructType::Span);
    span.add_child(StructChild::MarkedContentRef {
        mcid: 9,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    h.add_child(StructChild::StructElem(Box::new(span)));
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(h);
    let ordered = traverse_structure_tree(&tree, 0).unwrap();
    let item = ordered.iter().find(|c| c.mcid == Some(9)).unwrap();
    assert!(item.is_heading);
    assert_eq!(item.heading_level, Some(1));
}

/// D8b negative case — adjacent heading and body MCRs at the same
/// nesting level must keep their respective roles. A bug that
/// "leaked" heading flag from a prior sibling into the next would
/// flip every body paragraph after a heading into a heading.
#[test]
fn test_heading_role_does_not_bleed_into_following_paragraph() {
    let mut doc = StructElem::new(StructType::Document);
    let mut h1 = StructElem::new(StructType::H1);
    h1.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut p = StructElem::new(StructType::P);
    p.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    doc.add_child(StructChild::StructElem(Box::new(h1)));
    doc.add_child(StructChild::StructElem(Box::new(p)));
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(doc);

    let ordered = traverse_structure_tree(&tree, 0).unwrap();
    let h_item = ordered.iter().find(|c| c.mcid == Some(0)).unwrap();
    let p_item = ordered.iter().find(|c| c.mcid == Some(1)).unwrap();
    assert!(h_item.is_heading);
    assert!(!p_item.is_heading, "sibling P must not inherit H1's flag");
    assert_eq!(p_item.heading_level, None);
}

/// D8b coverage — list role variants on direct MCRs (LI carrying
/// its own MCR without LBody/Lbl wrappers) and LBody siblings
/// inside one LI.
#[test]
fn test_list_role_variants() {
    // Tree:
    // L
    //   ├─ LI (mcid=0, direct)         → role = LI
    //   └─ LI
    //        ├─ Lbl  (mcid=1)          → role = Lbl
    //        └─ LBody (mcid=2)         → role = LBody
    let mut l = StructElem::new(StructType::L);
    let mut li_a = StructElem::new(StructType::LI);
    li_a.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut li_b = StructElem::new(StructType::LI);
    let mut lbl = StructElem::new(StructType::Lbl);
    lbl.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut lbody = StructElem::new(StructType::LBody);
    lbody.add_child(StructChild::MarkedContentRef {
        mcid: 2,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    li_b.add_child(StructChild::StructElem(Box::new(lbl)));
    li_b.add_child(StructChild::StructElem(Box::new(lbody)));
    l.add_child(StructChild::StructElem(Box::new(li_a)));
    l.add_child(StructChild::StructElem(Box::new(li_b)));
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(l);

    let ordered = traverse_structure_tree(&tree, 0).unwrap();
    let m0 = ordered.iter().find(|c| c.mcid == Some(0)).unwrap();
    let m1 = ordered.iter().find(|c| c.mcid == Some(1)).unwrap();
    let m2 = ordered.iter().find(|c| c.mcid == Some(2)).unwrap();
    assert!(matches!(m0.list_role, Some(ListRole::LI)));
    assert!(matches!(m1.list_role, Some(ListRole::Lbl)));
    assert!(matches!(m2.list_role, Some(ListRole::LBody)));
    // None of the list MCRs are headings.
    assert!(!m0.is_heading && !m1.is_heading && !m2.is_heading);
}

/// D5 coverage at the traversal layer — block_id must increment
/// across sibling block elements but stay constant inside one
/// block, even when the block contains multiple Span children.
#[test]
fn test_block_id_groups_within_block_and_changes_across() {
    let mut doc = StructElem::new(StructType::Document);
    let mut p1 = StructElem::new(StructType::P);
    let mut span_a = StructElem::new(StructType::Span);
    span_a.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut span_b = StructElem::new(StructType::Span);
    span_b.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    p1.add_child(StructChild::StructElem(Box::new(span_a)));
    p1.add_child(StructChild::StructElem(Box::new(span_b)));
    let mut p2 = StructElem::new(StructType::P);
    p2.add_child(StructChild::MarkedContentRef {
        mcid: 2,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    doc.add_child(StructChild::StructElem(Box::new(p1)));
    doc.add_child(StructChild::StructElem(Box::new(p2)));
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(doc);

    let ordered = traverse_structure_tree(&tree, 0).unwrap();
    let m0 = ordered.iter().find(|c| c.mcid == Some(0)).unwrap();
    let m1 = ordered.iter().find(|c| c.mcid == Some(1)).unwrap();
    let m2 = ordered.iter().find(|c| c.mcid == Some(2)).unwrap();
    assert_eq!(
        m0.block_id, m1.block_id,
        "two MCRs inside the same /P must share block_id"
    );
    assert_ne!(
        m0.block_id, m2.block_id,
        "MCRs in different /P elements must have different block_id"
    );
    assert!(
        m0.block_id > 0,
        "block_id should be positive once any block is entered"
    );
}

/// D5 coverage — Span elements at the root (no enclosing block)
/// keep block_id=0 so the converter's "Some, Some, equal" check
/// stays well-defined.
#[test]
fn test_root_span_has_block_id_zero() {
    let mut span = StructElem::new(StructType::Span);
    span.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(span);
    let ordered = traverse_structure_tree(&tree, 0).unwrap();
    assert_eq!(ordered[0].block_id, 0);
}

#[test]
fn test_object_ref_skipped() {
    let mut root = StructElem::new(StructType::Document);
    root.add_child(StructChild::ObjectRef(42, 0));
    root.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    let order = extract_reading_order(&struct_tree, 0).unwrap();
    assert_eq!(order, vec![0]);
}

#[test]
fn test_traverse_all_pages() {
    let mut root = StructElem::new(StructType::Document);

    let mut p1 = StructElem::new(StructType::P);
    p1.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let mut p2 = StructElem::new(StructType::P);
    p2.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 1,
        scope: crate::structure::McidScope::Page(1),
    });

    let mut p3 = StructElem::new(StructType::P);
    p3.add_child(StructChild::MarkedContentRef {
        mcid: 2,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    root.add_child(StructChild::StructElem(Box::new(p1)));
    root.add_child(StructChild::StructElem(Box::new(p2)));
    root.add_child(StructChild::StructElem(Box::new(p3)));

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    let all_pages = traverse_structure_tree_all_pages(&struct_tree);
    assert_eq!(all_pages.len(), 2); // pages 0 and 1
    assert_eq!(all_pages[&0].len(), 2); // MCIDs 0 and 2
    assert_eq!(all_pages[&1].len(), 1); // MCID 1
}

#[test]
fn test_actual_text_descendants_recorded_for_assembler_suppression() {
    // The per-page traversal continues to record descendant MCIDs
    // when their ancestor carries /ActualText. The replacement
    // itself is resolved separately via `build_actualtext_index`
    // (so multi-page emit-once stays consistent across paths). The
    // assembler then suppresses the descendant MCID and emits the
    // replacement at the anchor's position.
    let mut root = StructElem::new(StructType::Document);
    let mut elem = StructElem::new(StructType::Span);
    elem.actual_text = Some("Replacement text".to_string());
    elem.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    root.add_child(StructChild::StructElem(Box::new(elem)));
    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    let ordered = traverse_structure_tree(&struct_tree, 0).unwrap();
    // Descendant MCID still present and uncoated; assembler drops
    // it via covered_mcids from the index.
    assert_eq!(ordered.len(), 1);
    assert_eq!(ordered[0].mcid, Some(0));
    assert_eq!(ordered[0].actual_text, None);

    // The replacement is resolved separately.
    let idx = build_actualtext_index(&struct_tree);
    assert!(idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(0), 0)));
    assert_eq!(
        idx.mcid_to_actual_text
            .get(&(crate::structure::McidScope::Page(0), 0))
            .map(|s| &**s),
        Some("Replacement text")
    );
}

#[test]
fn test_actual_text_wrong_page() {
    let mut root = StructElem::new(StructType::Document);

    let mut elem = StructElem::new(StructType::Span);
    elem.actual_text = Some("Replacement".to_string());
    elem.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 1,
        scope: crate::structure::McidScope::Page(1),
    });

    root.add_child(StructChild::StructElem(Box::new(elem)));

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    // Page 0 has no descendant MCID, so per-page traversal returns
    // empty. The index records the (page-1, MCID-0) coverage.
    let ordered = traverse_structure_tree(&struct_tree, 0).unwrap();
    assert!(ordered.is_empty());
    let idx = build_actualtext_index(&struct_tree);
    assert!(idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(1), 0)));
    assert_eq!(
        idx.mcid_to_actual_text
            .get(&(crate::structure::McidScope::Page(1), 0))
            .map(|s| &**s),
        Some("Replacement")
    );
}

#[test]
fn test_heading_and_block_flags() {
    let mut root = StructElem::new(StructType::Document);

    let mut h1 = StructElem::new(StructType::H1);
    h1.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let mut span = StructElem::new(StructType::Span);
    span.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    root.add_child(StructChild::StructElem(Box::new(h1)));
    root.add_child(StructChild::StructElem(Box::new(span)));

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    let ordered = traverse_structure_tree(&struct_tree, 0).unwrap();
    assert_eq!(ordered.len(), 2);
    assert!(ordered[0].is_heading);
    assert!(ordered[0].is_block);
    assert!(!ordered[1].is_heading);
    assert!(!ordered[1].is_block);
}

#[test]
fn test_collect_pages() {
    let mut elem = StructElem::new(StructType::Document);
    elem.page = Some(0);

    let mut child = StructElem::new(StructType::P);
    child.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 1,
        scope: crate::structure::McidScope::Page(1),
    });
    child.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 2,
        scope: crate::structure::McidScope::Page(2),
    });

    elem.add_child(StructChild::StructElem(Box::new(child)));

    let pages = collect_pages(&elem);
    assert_eq!(pages, vec![0, 1, 2]);
}

#[test]
fn test_traverse_all_pages_with_actual_text_does_not_repeat_per_page() {
    // Per the multi-page emit-once rule (PDF spec §14.9.4 positions
    // ActualText as a region replacement, not a per-page
    // repetition), the per-page traversal no longer carries
    // actual_text — instead it surfaces every descendant MCID so
    // the assembler can suppress them. The index records ONE
    // emission with first_page = 0.
    let mut root = StructElem::new(StructType::Document);
    let mut elem = StructElem::new(StructType::Span);
    elem.actual_text = Some("Hello".to_string());
    elem.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    elem.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 1,
        scope: crate::structure::McidScope::Page(1),
    });
    root.add_child(StructChild::StructElem(Box::new(elem)));
    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    let all_pages = traverse_structure_tree_all_pages(&struct_tree);
    assert!(all_pages.contains_key(&0));
    assert!(all_pages.contains_key(&1));
    // Descendant MCIDs surface on their own page, with no
    // actual_text on the OrderedContent itself.
    for items in all_pages.values() {
        for item in items {
            assert!(item.actual_text.is_none());
        }
    }
    let idx = build_actualtext_index(&struct_tree);
    // The bearing element covers both pages; first page wins for
    // emission, the second is suppress-only.
    assert!(idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(0), 0)));
    assert!(idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(1), 1)));
    assert!(idx
        .mcid_to_actual_text
        .contains_key(&(crate::structure::McidScope::Page(0), 0)));
    assert!(idx
        .suppress_only
        .contains(&(crate::structure::McidScope::Page(1), 1)));
}

#[test]
fn test_traverse_all_pages_word_break_with_children() {
    let mut root = StructElem::new(StructType::P);

    let mut wb = StructElem::new(StructType::WB);
    let mut child = StructElem::new(StructType::Span);
    child.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    wb.add_child(StructChild::StructElem(Box::new(child)));

    root.add_child(StructChild::StructElem(Box::new(wb)));

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    let all_pages = traverse_structure_tree_all_pages(&struct_tree);
    let page0 = &all_pages[&0];
    // Should have word break marker and the child's MCID
    assert!(page0.iter().any(|c| c.is_word_break));
    assert!(page0.iter().any(|c| c.mcid == Some(0)));
}

#[test]
fn test_traverse_all_pages_object_ref() {
    let mut root = StructElem::new(StructType::Document);
    root.add_child(StructChild::ObjectRef(99, 0));
    root.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let mut struct_tree = StructTreeRoot::new();
    struct_tree.add_root_element(root);

    let all_pages = traverse_structure_tree_all_pages(&struct_tree);
    assert_eq!(all_pages[&0].len(), 1);
    assert_eq!(all_pages[&0][0].mcid, Some(0));
}

#[test]
fn test_has_content_on_page_deep() {
    let mut root = StructElem::new(StructType::Document);
    let mut sect = StructElem::new(StructType::Sect);
    let mut p = StructElem::new(StructType::P);
    p.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 3,
        scope: crate::structure::McidScope::Page(3),
    });
    sect.add_child(StructChild::StructElem(Box::new(p)));
    root.add_child(StructChild::StructElem(Box::new(sect)));

    assert!(has_content_on_page(&root, 3));
    assert!(!has_content_on_page(&root, 0));
}

// === ActualTextIndex builder tests ===
//
// The builder satisfies these invariants per ISO 32000-1:2008 §14.9.4:
//   - Every (page, MCID) under an ActualText-bearing element is recorded
//     in `covered_mcids`.
//   - The bearing element's first page (min pre-order page of any
//     descendant MCR) is the emission page; that page's (page, mcid)
//     pairs land in `mcid_to_actual_text` with the innermost active
//     replacement.
//   - Pairs on non-first pages land in `suppress_only` to suppress
//     raw glyphs without re-emitting (emit-once across pages).
//   - When ActualText scopes nest, the inner replacement wins for
//     `(page, mcid)` keys the inner scope covers (recorded under
//     the inner scope's text in `mcid_to_actual_text`).
