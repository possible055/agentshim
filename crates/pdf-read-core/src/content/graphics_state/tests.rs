use super::*;

#[test]
fn test_matrix_identity() {
    let m = Matrix::identity();
    assert_eq!(m.a, 1.0);
    assert_eq!(m.b, 0.0);
    assert_eq!(m.c, 0.0);
    assert_eq!(m.d, 1.0);
    assert_eq!(m.e, 0.0);
    assert_eq!(m.f, 0.0);
}

#[test]
fn test_matrix_translation() {
    let m = Matrix::translation(10.0, 20.0);
    assert_eq!(m.e, 10.0);
    assert_eq!(m.f, 20.0);

    let p = m.transform_point(5.0, 10.0);
    assert_eq!(p.x, 15.0);
    assert_eq!(p.y, 30.0);
}

#[test]
fn test_matrix_scaling() {
    let m = Matrix::scaling(2.0, 3.0);
    assert_eq!(m.a, 2.0);
    assert_eq!(m.d, 3.0);

    let p = m.transform_point(10.0, 10.0);
    assert_eq!(p.x, 20.0);
    assert_eq!(p.y, 30.0);
}

#[test]
fn test_matrix_multiply() {
    let m1 = Matrix::translation(10.0, 20.0);
    let m2 = Matrix::scaling(2.0, 2.0);
    let result = m1.multiply(&m2);

    // m1.multiply(&m2) applies m2 first, then m1: first translate, then scale
    // So point (5,5) -> translate to (15,25) -> scale to (30,50)
    let p = result.transform_point(5.0, 5.0);
    assert_eq!(p.x, 30.0); // (5+10)*2
    assert_eq!(p.y, 50.0); // (5+20)*2
}

#[test]
fn test_matrix_multiply_order() {
    let m1 = Matrix::translation(10.0, 0.0);
    let m2 = Matrix::scaling(2.0, 1.0);

    let r1 = m1.multiply(&m2);
    let r2 = m2.multiply(&m1);

    // Different results show multiplication is not commutative
    let p = Point { x: 5.0, y: 0.0 };
    let p1 = r1.transform_point(p.x, p.y);
    let p2 = r2.transform_point(p.x, p.y);

    assert_ne!(p1.x, p2.x);
}

#[test]
fn test_matrix_determinant() {
    let m = Matrix::scaling(2.0, 3.0);
    assert_eq!(m.determinant(), 6.0);

    let m_identity = Matrix::identity();
    assert_eq!(m_identity.determinant(), 1.0);
}

#[test]
fn test_matrix_invertible() {
    let m = Matrix::scaling(2.0, 3.0);
    assert!(m.is_invertible());

    let m_degenerate = Matrix {
        a: 1.0,
        b: 2.0,
        c: 2.0,
        d: 4.0,
        e: 0.0,
        f: 0.0,
    };
    assert!(!m_degenerate.is_invertible());
}

/// `advance_text_matrix` in horizontal mode adds `displacement * (a, b)`
/// to the text matrix translation. This is the single hot-path math
/// used by every Tj/TJ operator extractor + measure-only path.
#[test]
fn test_advance_text_matrix_horizontal() {
    let mut gs = GraphicsState::new();
    gs.text_wmode = 0;
    gs.text_matrix = Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 100.0,
        f: 200.0,
    };
    let (de, df) = gs.advance_text_matrix(15.0);
    assert_eq!(de, 15.0);
    assert_eq!(df, 0.0);
    assert_eq!(gs.text_matrix.e, 115.0);
    assert_eq!(gs.text_matrix.f, 200.0);
}

/// In vertical mode the displacement multiplies `(c, d)` instead of
/// `(a, b)` per ISO 32000-1 §9.4.4. With an identity Tm `c = 0`, `d = 1`,
/// the cursor moves only in y.
#[test]
fn test_advance_text_matrix_vertical() {
    let mut gs = GraphicsState::new();
    gs.text_wmode = 1;
    gs.text_matrix = Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 100.0,
        f: 200.0,
    };
    let (de, df) = gs.advance_text_matrix(-12.0);
    assert_eq!(de, 0.0);
    assert_eq!(df, -12.0);
    assert_eq!(gs.text_matrix.e, 100.0);
    assert_eq!(gs.text_matrix.f, 188.0);
}

/// Sanity: a 90° rotation Tm in horizontal mode and the same `Tm` in
/// vertical mode produce different deltas. This is the math that
/// guarantees the rasterizer + extractor lay glyphs out along the
/// correct axis when the CTM is itself rotated (e.g., a label that
/// uses CTM rotation to stand text up vertically — that case must NOT
/// be conflated with WMode 1, which is a font-level signal).
#[test]
fn test_advance_text_matrix_rotated_matrix() {
    let rotated = Matrix {
        a: 0.0,
        b: 1.0,
        c: -1.0,
        d: 0.0,
        e: 0.0,
        f: 0.0,
    };
    let mut h = GraphicsState::new();
    h.text_wmode = 0;
    h.text_matrix = rotated;
    let (he, hf) = h.advance_text_matrix(10.0);
    assert_eq!((he, hf), (0.0, 10.0));

    let mut v = GraphicsState::new();
    v.text_wmode = 1;
    v.text_matrix = rotated;
    let (ve, vf) = v.advance_text_matrix(10.0);
    assert_eq!((ve, vf), (-10.0, 0.0));
}

#[test]
fn test_graphics_state_new() {
    let state = GraphicsState::new();
    assert_eq!(state.font_size, 12.0);
    assert_eq!(state.horizontal_scaling, 100.0);
    assert_eq!(state.char_space, 0.0);
    assert_eq!(state.word_space, 0.0);
    assert_eq!(state.leading, 0.0);
    assert!(state.font_name.is_none());
}

#[test]
fn test_graphics_state_default() {
    let state = GraphicsState::default();
    assert_eq!(state.font_size, 12.0);
}

#[test]
fn test_graphics_state_stack_new() {
    let stack = GraphicsStateStack::new();
    assert_eq!(stack.depth(), 1);
    assert_eq!(stack.current().font_size, 12.0);
}

#[test]
fn test_graphics_state_stack_save_restore() {
    let mut stack = GraphicsStateStack::new();

    // Modify current state
    stack.current_mut().font_size = 14.0;
    assert_eq!(stack.current().font_size, 14.0);

    // Save state
    stack.save();
    assert_eq!(stack.depth(), 2);
    assert_eq!(stack.current().font_size, 14.0);

    // Modify again
    stack.current_mut().font_size = 16.0;
    assert_eq!(stack.current().font_size, 16.0);

    // Restore
    stack.restore();
    assert_eq!(stack.depth(), 1);
    assert_eq!(stack.current().font_size, 14.0);
}

#[test]
fn test_graphics_state_stack_restore_limit() {
    let mut stack = GraphicsStateStack::new();
    assert_eq!(stack.depth(), 1);

    // Try to restore when only one state exists
    stack.restore();
    assert_eq!(stack.depth(), 1); // Should still have one state

    // Save and restore multiple times
    stack.save();
    stack.save();
    stack.save();
    assert_eq!(stack.depth(), 4);

    stack.restore();
    stack.restore();
    stack.restore();
    assert_eq!(stack.depth(), 1);

    // One more restore should have no effect
    stack.restore();
    assert_eq!(stack.depth(), 1);
}

#[test]
fn test_graphics_state_color() {
    let mut state = GraphicsState::new();
    assert_eq!(state.fill_color_rgb, (0.0, 0.0, 0.0));
    assert_eq!(state.stroke_color_rgb, (0.0, 0.0, 0.0));

    state.fill_color_rgb = (1.0, 0.0, 0.0);
    state.stroke_color_rgb = (0.0, 1.0, 0.0);

    assert_eq!(state.fill_color_rgb, (1.0, 0.0, 0.0));
    assert_eq!(state.stroke_color_rgb, (0.0, 1.0, 0.0));
}

#[test]
fn test_graphics_state_clone() {
    let mut state1 = GraphicsState::new();
    state1.font_size = 14.0;

    let state2 = state1.clone();
    assert_eq!(state2.font_size, 14.0);
}

#[test]
fn test_matrix_transform_origin() {
    let m = Matrix::identity();
    let p = m.transform_point(0.0, 0.0);
    assert_eq!(p.x, 0.0);
    assert_eq!(p.y, 0.0);
}

#[test]
fn test_matrix_default() {
    let m = Matrix::default();
    assert_eq!(m.a, 1.0);
    assert_eq!(m.d, 1.0);
}

#[test]
fn graphics_state_default_overprint_is_off() {
    // ISO 32000-1 Table 128: OP/op default false, OPM default 0.
    let gs = GraphicsState::default();
    assert!(!gs.fill_overprint);
    assert!(!gs.stroke_overprint);
    assert_eq!(gs.overprint_mode, 0);
}

#[test]
fn graphics_state_overprint_survives_save_restore() {
    let mut stack = GraphicsStateStack::new();
    stack.current_mut().fill_overprint = true;
    stack.current_mut().stroke_overprint = true;
    stack.current_mut().overprint_mode = 1;

    stack.save();
    stack.current_mut().fill_overprint = false;
    stack.current_mut().stroke_overprint = false;
    stack.current_mut().overprint_mode = 0;

    stack.restore();
    assert!(stack.current().fill_overprint);
    assert!(stack.current().stroke_overprint);
    assert_eq!(stack.current().overprint_mode, 1);
}
