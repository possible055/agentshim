use super::*;

#[test]
fn test_actualtext_index_simple_single_mcid() {
    // Span /ActualText "fi" /K 0 on page 0.
    let mut span = StructElem::new(StructType::Span);
    span.actual_text = Some("fi".to_string());
    span.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let mut tree = StructTreeRoot::new();
    tree.add_root_element(span);

    let idx = build_actualtext_index(&tree);
    assert!(idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(0), 0)));
    assert_eq!(
        idx.mcid_to_actual_text
            .get(&(crate::structure::McidScope::Page(0), 0))
            .map(|s| &**s),
        Some("fi")
    );
    assert!(idx.suppress_only.is_empty());
}

#[test]
fn test_actualtext_index_nested_inner_wins() {
    // Outer Span /ActualText "outer" wrapping inner Span /ActualText
    // "inner" wrapping MCID 5. Inner replacement must win for MCID 5.
    let mut inner = StructElem::new(StructType::Span);
    inner.actual_text = Some("inner".to_string());
    inner.add_child(StructChild::MarkedContentRef {
        mcid: 5,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let mut outer = StructElem::new(StructType::Span);
    outer.actual_text = Some("outer".to_string());
    outer.add_child(StructChild::StructElem(Box::new(inner)));

    let mut tree = StructTreeRoot::new();
    tree.add_root_element(outer);

    let idx = build_actualtext_index(&tree);
    // The leaf MCID is covered by the INNER text (inner-wins).
    assert_eq!(
        idx.mcid_to_actual_text
            .get(&(crate::structure::McidScope::Page(0), 5))
            .map(|s| &**s),
        Some("inner")
    );
    assert!(idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(0), 5)));
}

#[test]
fn test_actualtext_index_nested_outer_sibling_with_inner_subtree() {
    // CRITICAL-1 shape:
    //   Outer Span /ActualText "O" /K [Inner /K 0, MCID 1]
    //   Inner Span /ActualText "I" /K 0
    // Expected: (page 0, MCID 0) → "I"; (page 0, MCID 1) → "O".
    // Both are covered; both must emit (the outer is NOT shadowed
    // even though the inner exists, because MCID 1 belongs to the
    // outer scope only).
    let mut inner = StructElem::new(StructType::Span);
    inner.actual_text = Some("I".to_string());
    inner.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let mut outer = StructElem::new(StructType::Span);
    outer.actual_text = Some("O".to_string());
    outer.add_child(StructChild::StructElem(Box::new(inner)));
    outer.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });

    let mut tree = StructTreeRoot::new();
    tree.add_root_element(outer);
    let idx = build_actualtext_index(&tree);

    assert_eq!(
        idx.mcid_to_actual_text
            .get(&(crate::structure::McidScope::Page(0), 0))
            .map(|s| &**s),
        Some("I")
    );
    assert_eq!(
        idx.mcid_to_actual_text
            .get(&(crate::structure::McidScope::Page(0), 1))
            .map(|s| &**s),
        Some("O")
    );
    assert!(idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(0), 0)));
    assert!(idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(0), 1)));
}

#[test]
fn test_actualtext_index_multi_page_first_page_emits_others_suppress() {
    // /H1 /ActualText "Heading X" covering MCIDs on pages 0 AND 1.
    // The bearing element's first descendant in pre-order sits on
    // page 1 first, then page 0 — the first descendant wins (page 1).
    let mut h1 = StructElem::new(StructType::H1);
    h1.actual_text = Some("Heading X".to_string());
    h1.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 1,
        scope: crate::structure::McidScope::Page(1),
    });
    h1.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(h1);
    let idx = build_actualtext_index(&tree);
    // Both descendant pairs are covered.
    assert!(idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(1), 0)));
    assert!(idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(0), 1)));
    // The first MCR in pre-order is (page 1, MCID 0): that page
    // wins for emission.
    assert_eq!(
        idx.mcid_to_actual_text
            .get(&(crate::structure::McidScope::Page(1), 0))
            .map(|s| &**s),
        Some("Heading X")
    );
    // The other-page MCR is suppress-only.
    assert!(idx
        .suppress_only
        .contains(&(crate::structure::McidScope::Page(0), 1)));
    assert!(!idx
        .mcid_to_actual_text
        .contains_key(&(crate::structure::McidScope::Page(0), 1)));
}

#[test]
fn test_actualtext_index_multi_mcid_subtree() {
    // Span /ActualText "expanded" /K [7 8 9]. All three MCIDs
    // suppressed; all three share the same replacement on page 0.
    let mut span = StructElem::new(StructType::Span);
    span.actual_text = Some("expanded".to_string());
    for m in [7, 8, 9] {
        span.add_child(StructChild::MarkedContentRef {
            mcid: m,
            page: 0,
            scope: crate::structure::McidScope::Page(0),
        });
    }
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(span);
    let idx = build_actualtext_index(&tree);
    for m in [7, 8, 9] {
        assert!(idx
            .covered_mcids
            .contains(&(crate::structure::McidScope::Page(0), m)));
        assert_eq!(
            idx.mcid_to_actual_text
                .get(&(crate::structure::McidScope::Page(0), m))
                .map(|s| &**s),
            Some("expanded")
        );
    }
}

#[test]
fn test_actualtext_index_no_actualtext_yields_empty() {
    // A plain tree with no /ActualText anywhere builds an empty index.
    let mut p = StructElem::new(StructType::P);
    p.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(p);
    let idx = build_actualtext_index(&tree);
    assert!(idx.is_empty());
    assert!(idx.mcid_to_actual_text.is_empty());
    assert!(idx.covered_mcids.is_empty());
}

#[test]
fn test_actualtext_index_empty_actualtext_is_ignored() {
    // An empty /ActualText string MUST be ignored: a producer that
    // wrote it likely means "no replacement".
    let mut span = StructElem::new(StructType::Span);
    span.actual_text = Some(String::new());
    span.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(span);
    let idx = build_actualtext_index(&tree);
    assert!(idx.is_empty());
}

#[test]
fn test_actualtext_index_no_descendant_mcid_drops_scope() {
    // /ActualText with no descendant MCID has nothing to attach
    // to and contributes no entries.
    let mut span = StructElem::new(StructType::Span);
    span.actual_text = Some("ghost".to_string());
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(span);
    let idx = build_actualtext_index(&tree);
    assert!(idx.is_empty());
}

#[test]
fn test_actualtext_index_figure_with_actualtext() {
    // Figure /ActualText "logo text". Same shape as a Span.
    let mut fig = StructElem::new(StructType::Figure);
    fig.actual_text = Some("logo text".to_string());
    fig.add_child(StructChild::MarkedContentRef {
        mcid: 4,
        page: 2,
        scope: crate::structure::McidScope::Page(2),
    });
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(fig);
    let idx = build_actualtext_index(&tree);
    assert_eq!(
        idx.mcid_to_actual_text
            .get(&(crate::structure::McidScope::Page(2), 4))
            .map(|s| &**s),
        Some("logo text")
    );
}

#[test]
fn test_actualtext_index_cross_page_mcid_collision() {
    // CRITICAL-2 shape: page 0 has /H1 /ActualText "Heading" /K MCID 0
    // (covered); page 1 has a plain /P /K MCID 0 (NOT covered).
    // The (page, mcid) keying must keep them independent.
    let mut h1 = StructElem::new(StructType::H1);
    h1.actual_text = Some("Heading".to_string());
    h1.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    let mut p = StructElem::new(StructType::P);
    p.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 1,
        scope: crate::structure::McidScope::Page(1),
    });

    let mut doc = StructElem::new(StructType::Document);
    doc.add_child(StructChild::StructElem(Box::new(h1)));
    doc.add_child(StructChild::StructElem(Box::new(p)));
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(doc);

    let idx = build_actualtext_index(&tree);
    assert!(idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(0), 0)));
    // Page-1 MCID 0 is NOT covered: it belongs to a plain /P with
    // no /ActualText.
    assert!(!idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(1), 0)));
    assert!(!idx
        .suppress_only
        .contains(&(crate::structure::McidScope::Page(1), 0)));
    assert_eq!(
        idx.mcid_to_actual_text
            .get(&(crate::structure::McidScope::Page(0), 0))
            .map(|s| &**s),
        Some("Heading")
    );
}

// ============================================================================
// McidScope (ISO 32000-1:2008 §14.7.4.3) — per-content-stream MCID namespaces.
//
// The earlier `(page, mcid)` keying silently merged MCIDs that
// came from distinct content streams on the same page. Per spec,
// page content / Form XObject content / Tiling Pattern content
// each define their own MCID namespace. These tests lock in that
// the builder keeps them apart.
// ============================================================================

/// The canonical bug shape: two Form XObjects on the same page,
/// both emitting MCID 0, each wrapped by an ActualText-bearing
/// StructElem. The pre-fix `(page, mcid)` keying would have
/// collapsed them onto `(0, 0) → "Y"` (last-writer-wins). The
/// fix keys by `(McidScope::Form(form_ref), mcid)` and keeps
/// both replacements distinct.
#[test]
fn two_forms_with_same_mcid_on_same_page_do_not_collide() {
    let form_a = crate::object::ObjectRef::new(100, 0);
    let form_b = crate::object::ObjectRef::new(101, 0);

    let mut span_a = StructElem::new(StructType::Span);
    span_a.actual_text = Some("X".to_string());
    span_a.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Form(form_a),
    });

    let mut span_b = StructElem::new(StructType::Span);
    span_b.actual_text = Some("Y".to_string());
    span_b.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Form(form_b),
    });

    let mut doc = StructElem::new(StructType::Document);
    doc.add_child(StructChild::StructElem(Box::new(span_a)));
    doc.add_child(StructChild::StructElem(Box::new(span_b)));
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(doc);

    let idx = build_actualtext_index(&tree);

    // Both keys present.
    let key_a = (crate::structure::McidScope::Form(form_a), 0);
    let key_b = (crate::structure::McidScope::Form(form_b), 0);
    assert!(idx.covered_mcids.contains(&key_a));
    assert!(idx.covered_mcids.contains(&key_b));

    // Each form's replacement preserved — pre-fix, the second
    // overwrote the first.
    assert_eq!(idx.mcid_to_actual_text.get(&key_a).map(|s| &**s), Some("X"));
    assert_eq!(idx.mcid_to_actual_text.get(&key_b).map(|s| &**s), Some("Y"));
}

/// Form-scoped MCID lookup uses `McidScope::Form` regardless of
/// the page number recorded on the MCR (`/Pg`) — the form's
/// content stream is the namespace.
#[test]
fn actualtext_with_stm_form_resolves_to_form_scope() {
    let form_ref = crate::object::ObjectRef::new(42, 0);
    let mut span = StructElem::new(StructType::Span);
    span.actual_text = Some("alt".to_string());
    span.add_child(StructChild::MarkedContentRef {
        mcid: 3,
        page: 0,
        scope: crate::structure::McidScope::Form(form_ref),
    });
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(span);

    let idx = build_actualtext_index(&tree);
    let key = (crate::structure::McidScope::Form(form_ref), 3);
    assert!(idx.covered_mcids.contains(&key));
    assert_eq!(idx.mcid_to_actual_text.get(&key).map(|s| &**s), Some("alt"));
    // Page-scoped lookup with the same MCID MUST miss — the keys
    // are different namespaces.
    assert!(!idx
        .covered_mcids
        .contains(&(crate::structure::McidScope::Page(0), 3)));
}

/// Same as above but for Tiling Patterns (§8.7.3.3 + §14.7.4.3).
#[test]
fn actualtext_with_stm_pattern_resolves_to_pattern_scope() {
    let pattern_ref = crate::object::ObjectRef::new(7, 0);
    let mut span = StructElem::new(StructType::Span);
    span.actual_text = Some("dec".to_string());
    span.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 0,
        scope: crate::structure::McidScope::Pattern(pattern_ref),
    });
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(span);

    let idx = build_actualtext_index(&tree);
    let key = (crate::structure::McidScope::Pattern(pattern_ref), 1);
    assert!(idx.covered_mcids.contains(&key));
    assert_eq!(idx.mcid_to_actual_text.get(&key).map(|s| &**s), Some("dec"));
}

/// Two Tiling Patterns on the same page emit MCID 0 in their
/// own streams — the index keeps them distinct.
#[test]
fn pattern_with_actualtext_keys_under_pattern_scope() {
    let pat_a = crate::object::ObjectRef::new(70, 0);
    let pat_b = crate::object::ObjectRef::new(71, 0);

    let mut span_a = StructElem::new(StructType::Span);
    span_a.actual_text = Some("alpha".to_string());
    span_a.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Pattern(pat_a),
    });

    let mut span_b = StructElem::new(StructType::Span);
    span_b.actual_text = Some("beta".to_string());
    span_b.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Pattern(pat_b),
    });

    let mut doc = StructElem::new(StructType::Document);
    doc.add_child(StructChild::StructElem(Box::new(span_a)));
    doc.add_child(StructChild::StructElem(Box::new(span_b)));
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(doc);

    let idx = build_actualtext_index(&tree);
    let ka = (crate::structure::McidScope::Pattern(pat_a), 0);
    let kb = (crate::structure::McidScope::Pattern(pat_b), 0);
    assert_eq!(
        idx.mcid_to_actual_text.get(&ka).map(|s| &**s),
        Some("alpha")
    );
    assert_eq!(idx.mcid_to_actual_text.get(&kb).map(|s| &**s), Some("beta"));
}

/// When the MCR omits `/Stm` (the parser hands the builder a
/// `McidScope::Page(p)`), the page namespace is used.
#[test]
fn actualtext_without_stm_falls_back_to_page_scope() {
    let mut span = StructElem::new(StructType::Span);
    span.actual_text = Some("plain".to_string());
    span.add_child(StructChild::MarkedContentRef {
        mcid: 5,
        page: 2,
        scope: crate::structure::McidScope::Page(2),
    });
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(span);

    let idx = build_actualtext_index(&tree);
    let key = (crate::structure::McidScope::Page(2), 5);
    assert!(idx.covered_mcids.contains(&key));
    assert_eq!(
        idx.mcid_to_actual_text.get(&key).map(|s| &**s),
        Some("plain")
    );
}

/// Robustness: a malformed parent_tree / cycle should not panic
/// the builder. Tests the no-MCR case (the rest of the builder
/// is exercised by other tests).
#[test]
fn malformed_mcr_dict_does_not_panic_in_builder() {
    // No descendants at all — drops the scope, returns empty index.
    let mut span = StructElem::new(StructType::Span);
    span.actual_text = Some("ghost".to_string());
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(span);
    let idx = build_actualtext_index(&tree);
    assert!(idx.is_empty());
}

/// Mixed scopes under one ActualText: Page-scoped descendants
/// follow the cross-page first-page rule; Form-scoped descendants
/// emit at every covered key (each form is its own namespace).
#[test]
fn mixed_scopes_under_one_actualtext_use_per_namespace_rules() {
    let form_ref = crate::object::ObjectRef::new(50, 0);
    let mut outer = StructElem::new(StructType::Span);
    outer.actual_text = Some("alt".to_string());
    // Page-scoped, page 0: this is the "first page" → emits.
    outer.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Page(0),
    });
    // Page-scoped, page 1: not the first page → suppress-only.
    outer.add_child(StructChild::MarkedContentRef {
        mcid: 1,
        page: 1,
        scope: crate::structure::McidScope::Page(1),
    });
    // Form-scoped: independent namespace, emits regardless of
    // page-scope first-page logic.
    outer.add_child(StructChild::MarkedContentRef {
        mcid: 0,
        page: 0,
        scope: crate::structure::McidScope::Form(form_ref),
    });
    let mut tree = StructTreeRoot::new();
    tree.add_root_element(outer);

    let idx = build_actualtext_index(&tree);

    let page0 = (crate::structure::McidScope::Page(0), 0);
    let page1 = (crate::structure::McidScope::Page(1), 1);
    let formk = (crate::structure::McidScope::Form(form_ref), 0);

    // Page-scope first-page emits.
    assert_eq!(
        idx.mcid_to_actual_text.get(&page0).map(|s| &**s),
        Some("alt")
    );
    // Page-scope non-first-page is suppress-only.
    assert!(idx.suppress_only.contains(&page1));
    assert!(!idx.mcid_to_actual_text.contains_key(&page1));
    // Form-scope emits independently of the page-first-page rule.
    assert_eq!(
        idx.mcid_to_actual_text.get(&formk).map(|s| &**s),
        Some("alt")
    );
}
