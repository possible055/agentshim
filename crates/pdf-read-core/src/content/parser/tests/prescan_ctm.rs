use super::*;

// ── parse_and_execute_text_only ─────────────────────────────────

#[test]
fn test_parse_and_execute_text_only_basic() {
    let stream = b"BT /F1 12 Tf (Hello) Tj ET";
    let mut ops = Vec::new();
    parse_and_execute_text_only(stream, |op| {
        ops.push(op);
        Ok(())
    })
    .unwrap();
    assert!(ops.len() >= 3);
    assert!(matches!(ops[0], Operator::BeginText));
}

#[test]
fn test_parse_and_execute_text_only_skips_graphics() {
    let stream = b"100 200 m 300 400 l S BT (Hello) Tj ET";
    let mut ops = Vec::new();
    parse_and_execute_text_only(stream, |op| {
        ops.push(op);
        Ok(())
    })
    .unwrap();
    // Should skip path operators
    assert!(!ops.iter().any(|op| matches!(op, Operator::MoveTo { .. })));
    assert!(ops.iter().any(|op| matches!(op, Operator::Tj { .. })));
}

#[test]
fn test_parse_and_execute_text_only_empty() {
    let mut ops = Vec::new();
    parse_and_execute_text_only(b"", |op| {
        ops.push(op);
        Ok(())
    })
    .unwrap();
    assert!(ops.is_empty());
}

#[test]
fn test_prescan_preserves_outer_ctm_for_deeply_nested_text() {
    // Regression test for issue #265: prescan_text_regions loses outer CTM
    // context when text is deeply nested in graphics state with scaling cm
    // operators separated by >4KB of path data.
    //
    // Build a >256KB stream with:
    //   q / cm (scaling) / >4KB of filler / q / cm (translation) / BT...ET / Q / Q
    //
    // The forward CTM scan should capture the outer scaling and inject it
    // as part of the accumulated CTM before the text region. The injected
    // Cm should reflect the combined outer scaling × inner translation.
    let mut cs = Vec::new();

    // Outer graphics state with scaling
    cs.extend_from_slice(b"q\n");
    cs.extend_from_slice(b"0.1 0 0 0.1 50 50 cm\n");

    // >260KB of path filler to exceed the 256KB prescan threshold
    // and push the outer cm beyond the 4KB backward scan limit
    for i in 0..13000u32 {
        let line = format!(
            "{}.0 {}.0 m {}.0 {}.0 l n\n",
            i % 500,
            (i * 7) % 500,
            (i * 3) % 500,
            (i * 11) % 500
        );
        cs.extend_from_slice(line.as_bytes());
    }
    assert!(
        cs.len() > 256 * 1024,
        "stream must exceed 256KB prescan threshold"
    );

    // Nested text with large-coordinate translation (scaled by outer 0.1x)
    cs.extend_from_slice(b"q\n");
    cs.extend_from_slice(b"1 0 0 1 3000 4000 cm\n");
    cs.extend_from_slice(b"BT\n");
    cs.extend_from_slice(b"/F1 80 Tf\n");
    cs.extend_from_slice(b"(Test) Tj\n");
    cs.extend_from_slice(b"ET\n");
    cs.extend_from_slice(b"Q\n");
    cs.extend_from_slice(b"Q\n");

    let mut ops = Vec::new();
    parse_and_execute_text_only(&cs, |op| {
        ops.push(op);
        Ok(())
    })
    .unwrap();

    // The injected Cm should reflect the accumulated CTM at the BT position:
    // outer scaling (0.1) × inner translation (3000, 4000).
    // Net: a=0.1, d=0.1, e=0.1*3000+50=350, f=0.1*4000+50=450
    let cm_ops: Vec<_> = ops
        .iter()
        .filter(|op| matches!(op, Operator::Cm { .. }))
        .collect();
    assert!(
        !cm_ops.is_empty(),
        "expected at least 1 Cm operator (injected CTM)"
    );

    // Verify the injected Cm includes the outer 0.1x scaling
    let has_scaled_cm = cm_ops.iter().any(|op| {
        matches!(op, Operator::Cm { a, d, .. } if (*a - 0.1).abs() < 0.01 && (*d - 0.1).abs() < 0.02)
    });
    assert!(
        has_scaled_cm,
        "injected Cm should include outer 0.1x scaling, got: {:?}",
        cm_ops
    );
}

#[test]
fn test_prescan_forward_ctm_injects_font_state() {
    // When BT blocks share font state (second BT has no Tf), the forward
    // CTM scan should track the font and inject it so font_size is correct.
    let mut cs = Vec::new();

    cs.extend_from_slice(b"q\n");
    cs.extend_from_slice(b"0.1 0 0 0.1 50 50 cm\n");

    // Filler to exceed 256KB prescan threshold
    for i in 0..13000u32 {
        let line = format!(
            "{}.0 {}.0 m {}.0 {}.0 l n\n",
            i % 500,
            (i * 7) % 500,
            (i * 3) % 500,
            (i * 11) % 500
        );
        cs.extend_from_slice(line.as_bytes());
    }
    assert!(cs.len() > 256 * 1024);

    // First BT sets the font
    cs.extend_from_slice(b"q\n");
    cs.extend_from_slice(b"1 0 0 1 1000 2000 cm\n");
    cs.extend_from_slice(b"BT\n");
    cs.extend_from_slice(b"/F1 80 Tf\n");
    cs.extend_from_slice(b"(First) Tj\n");
    cs.extend_from_slice(b"ET\n");

    // Second BT inherits font (no Tf) — same q scope
    cs.extend_from_slice(b"BT\n");
    cs.extend_from_slice(b"(Second) Tj\n");
    cs.extend_from_slice(b"ET\n");
    cs.extend_from_slice(b"Q\n");
    cs.extend_from_slice(b"Q\n");

    let mut ops = Vec::new();
    parse_and_execute_text_only(&cs, |op| {
        ops.push(op);
        Ok(())
    })
    .unwrap();

    // Both BT blocks should have a Tf injected (either explicit or from forward scan)
    let tf_ops: Vec<_> = ops
        .iter()
        .filter(|op| matches!(op, Operator::Tf { .. }))
        .collect();
    assert!(
        tf_ops.len() >= 2,
        "expected Tf for both BT blocks (explicit + inherited), got {}",
        tf_ops.len()
    );
}

#[test]
fn test_prescan_forward_ctm_restores_after_nested_scope() {
    // After a q/cm/Q block, the CTM should be restored. Text after the
    // nested scope should get the outer CTM, not the inner one.
    let mut cs = Vec::new();

    cs.extend_from_slice(b"q\n");
    cs.extend_from_slice(b"1 0 0 1 0 0 cm\n"); // identity (outer)

    // Filler to exceed 256KB
    for i in 0..13000u32 {
        let line = format!(
            "{}.0 {}.0 m {}.0 {}.0 l n\n",
            i % 500,
            (i * 7) % 500,
            (i * 3) % 500,
            (i * 11) % 500
        );
        cs.extend_from_slice(line.as_bytes());
    }
    assert!(cs.len() > 256 * 1024);

    // Nested scope with large scaling
    cs.extend_from_slice(b"q\n");
    cs.extend_from_slice(b"10 0 0 10 0 0 cm\n"); // 10x scale
    cs.extend_from_slice(b"BT\n");
    cs.extend_from_slice(b"/F1 12 Tf\n");
    cs.extend_from_slice(b"(Scaled) Tj\n");
    cs.extend_from_slice(b"ET\n");
    cs.extend_from_slice(b"Q\n"); // restore — 10x scale gone

    // Text after the nested scope should NOT have 10x scale
    cs.extend_from_slice(b"BT\n");
    cs.extend_from_slice(b"/F1 12 Tf\n");
    cs.extend_from_slice(b"(Normal) Tj\n");
    cs.extend_from_slice(b"ET\n");
    cs.extend_from_slice(b"Q\n");

    let mut ops = Vec::new();
    parse_and_execute_text_only(&cs, |op| {
        ops.push(op);
        Ok(())
    })
    .unwrap();

    // Collect all injected Cm operators
    let cm_ops: Vec<_> = ops
        .iter()
        .filter_map(|op| {
            if let Operator::Cm { a, .. } = op {
                Some(*a)
            } else {
                None
            }
        })
        .collect();

    // Should have at least 2 Cm injections (one for each BT region)
    assert!(
        cm_ops.len() >= 2,
        "expected at least 2 Cm operators, got {:?}",
        cm_ops
    );

    // The second Cm (for "Normal" text) should NOT have the 10x scale.
    // It should be ~1.0 (identity outer CTM).
    let last_cm = cm_ops.last().unwrap();
    assert!(
        (*last_cm - 1.0).abs() < 0.1,
        "CTM after nested Q should be restored to ~1.0, got {}",
        last_cm
    );
}

#[test]
fn test_prescan_forward_ctm_multiple_depths() {
    // Multiple BT blocks at different nesting depths should each get
    // the correct CTM for their position in the graphics state stack.
    let mut cs = Vec::new();

    cs.extend_from_slice(b"q\n");
    cs.extend_from_slice(b"0.5 0 0 0.5 100 100 cm\n"); // outer: 0.5x scale + translate

    // Filler
    for i in 0..13000u32 {
        let line = format!(
            "{}.0 {}.0 m {}.0 {}.0 l n\n",
            i % 500,
            (i * 7) % 500,
            (i * 3) % 500,
            (i * 11) % 500
        );
        cs.extend_from_slice(line.as_bytes());
    }
    assert!(cs.len() > 256 * 1024);

    // BT at depth 1 (inside 0.5x scaling)
    cs.extend_from_slice(b"BT\n");
    cs.extend_from_slice(b"/F1 20 Tf\n");
    cs.extend_from_slice(b"(Depth1) Tj\n");
    cs.extend_from_slice(b"ET\n");

    // BT at depth 2 (inside 0.5x + additional 2x = net 1x)
    cs.extend_from_slice(b"q\n");
    cs.extend_from_slice(b"2 0 0 2 0 0 cm\n");
    cs.extend_from_slice(b"BT\n");
    cs.extend_from_slice(b"/F1 20 Tf\n");
    cs.extend_from_slice(b"(Depth2) Tj\n");
    cs.extend_from_slice(b"ET\n");
    cs.extend_from_slice(b"Q\n");

    cs.extend_from_slice(b"Q\n");

    let mut ops = Vec::new();
    parse_and_execute_text_only(&cs, |op| {
        ops.push(op);
        Ok(())
    })
    .unwrap();

    let cm_ops: Vec<_> = ops
        .iter()
        .filter_map(|op| {
            if let Operator::Cm { a, .. } = op {
                Some(*a)
            } else {
                None
            }
        })
        .collect();

    assert!(
        cm_ops.len() >= 2,
        "expected at least 2 Cm operators for 2 BT blocks, got {:?}",
        cm_ops
    );

    // First Cm should have scale ~0.5 (outer only)
    assert!(
        (cm_ops[0] - 0.5).abs() < 0.05,
        "First BT should get outer 0.5x CTM, got {}",
        cm_ops[0]
    );

    // Second Cm should have scale ~1.0 (0.5 × 2.0)
    assert!(
        (cm_ops[1] - 1.0).abs() < 0.1,
        "Second BT should get composed 0.5×2.0=1.0 CTM, got {}",
        cm_ops[1]
    );
}
