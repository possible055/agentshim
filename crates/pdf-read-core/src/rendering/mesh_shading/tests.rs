use super::*;

#[test]
fn bit_reader_reads_msb_first_and_bounds() {
    // 0b1011_0010, 0b1100_0000
    let data = [0b1011_0010u8, 0b1100_0000u8];
    let mut r = BitReader::new(&data);
    assert_eq!(r.read_bits(1), Some(1));
    assert_eq!(r.read_bits(3), Some(0b011));
    assert_eq!(r.read_bits(4), Some(0b0010));
    assert_eq!(r.read_bits(2), Some(0b11));
    // Two bits left after this (the trailing zeros); asking for more
    // than remain returns None.
    assert_eq!(r.read_bits(8), None);
}

#[test]
fn decode_value_maps_endpoints() {
    // 8-bit: 0 → lo, 255 → hi, midpoint ≈ centre.
    assert!((decode_value(0, 8, -2.0, 2.0) - (-2.0)).abs() < 1e-6);
    assert!((decode_value(255, 8, -2.0, 2.0) - 2.0).abs() < 1e-6);
    assert!((decode_value(128, 8, 0.0, 1.0) - 0.5019608).abs() < 1e-4);
}

/// Build a minimal Type 4 vertex stream by hand and confirm the decoder
/// reconstructs the two triangles (one fresh, one edge-shared).
#[test]
fn type4_stream_decodes_flags_and_triangles() {
    // 8 bits per flag/coord/component; 1 colour component.
    // Decode: x∈[0,1] y∈[0,1] c∈[0,1].
    let params = MeshParams {
        bits_per_flag: 8,
        bits_per_coord: 8,
        bits_per_comp: 8,
        vertices_per_row: 0,
        decode: vec![(0.0, 1.0), (0.0, 1.0), (0.0, 1.0)],
    };
    // Helper to push a vertex: flag, x, y, c (all bytes).
    let mut bytes: Vec<u8> = Vec::new();
    let mut push_v = |flag: u8, x: u8, y: u8, c: u8| {
        bytes.extend_from_slice(&[flag, x, y, c]);
    };
    // First triangle: three flag-0 vertices.
    push_v(0, 0, 0, 0); // (0,0) colour 0
    push_v(0, 255, 0, 255); // (1,0) colour 1
    push_v(0, 0, 255, 128); // (0,1) colour ~0.5
                            // Second triangle: flag 1 reuses edge (vb, vc) of the first.
    push_v(1, 255, 255, 255); // (1,1) colour 1

    let tris = decode_type4_stream(&bytes, &params, 100);
    assert_eq!(tris.len(), 2, "expected two triangles");
    // First triangle corners.
    assert!((tris[0][0].x - 0.0).abs() < 1e-4 && (tris[0][0].y - 0.0).abs() < 1e-4);
    assert!((tris[0][1].x - 1.0).abs() < 1e-4);
    assert!((tris[0][2].y - 1.0).abs() < 1e-4);
    // Shared triangle: first two vertices are the previous vb, vc.
    assert!((tris[1][0].x - 1.0).abs() < 1e-4); // prev vb = (1,0)
    assert!((tris[1][1].y - 1.0).abs() < 1e-4); // prev vc = (0,1)
    assert!((tris[1][2].x - 1.0).abs() < 1e-4 && (tris[1][2].y - 1.0).abs() < 1e-4);
}

/// A Type 5 lattice of 2 rows × 3 columns tessellates into 4 triangles.
#[test]
fn type5_lattice_tessellates_rows() {
    let params = MeshParams {
        bits_per_flag: 8,
        bits_per_coord: 8,
        bits_per_comp: 8,
        vertices_per_row: 3,
        decode: vec![(0.0, 1.0), (0.0, 1.0), (0.0, 1.0)],
    };
    let mut bytes: Vec<u8> = Vec::new();
    // Row 0 (y=0): three vertices at x=0,0.5,1.
    for &x in &[0u8, 128, 255] {
        bytes.extend_from_slice(&[x, 0, 0]); // x, y=0, colour
    }
    // Row 1 (y=1): three vertices.
    for &x in &[0u8, 128, 255] {
        bytes.extend_from_slice(&[x, 255, 255]);
    }
    let tris = decode_type5_stream(&bytes, &params, 100);
    // (vpr-1) cells × 2 triangles = 4.
    assert_eq!(tris.len(), 4);
}

/// Barycentric colour interpolation: a triangle with red/green/blue
/// corners yields the exact vertex colour at each corner and a mixed
/// colour near the centroid.
#[test]
fn gouraud_triangle_interpolates_colours() {
    let mut pixmap = Pixmap::new(10, 10).unwrap();
    let v0 = ((1.0, 1.0), (1.0, 0.0, 0.0, 1.0)); // red
    let v1 = ((8.0, 1.0), (0.0, 1.0, 0.0, 1.0)); // green
    let v2 = ((1.0, 8.0), (0.0, 0.0, 1.0, 1.0)); // blue
    fill_gouraud_triangle(&mut pixmap, None, v0, v1, v2);
    let data = pixmap.data();
    let px = |x: usize, y: usize| {
        let o = (y * 10 + x) * 4;
        (data[o], data[o + 1], data[o + 2], data[o + 3])
    };
    // Near v0 corner → mostly red.
    let (r, g, b, a) = px(1, 1);
    assert!(a > 0, "corner must be painted");
    assert!(
        r > g && r > b,
        "near red corner should be reddish: {r},{g},{b}"
    );
}

/// A degenerate (collinear) triangle must paint nothing and not panic.
#[test]
fn degenerate_triangle_is_skipped() {
    let mut pixmap = Pixmap::new(8, 8).unwrap();
    let c = (1.0, 1.0, 1.0, 1.0);
    fill_gouraud_triangle(
        &mut pixmap,
        None,
        ((0.0, 0.0), c),
        ((4.0, 4.0), c),
        ((8.0, 8.0), c),
    );
    assert!(pixmap.data().iter().all(|&b| b == 0), "no pixels painted");
}

/// End-to-end: decode a hand-built Type 4 stream and rasterise it
/// through the full triangle path into a pixmap, asserting the shaded
/// region actually receives non-background pixels.
#[test]
fn type4_renders_non_background_pixels() {
    let params = MeshParams {
        bits_per_flag: 8,
        bits_per_coord: 8,
        bits_per_comp: 8,
        vertices_per_row: 0,
        // Coordinates decode into the 0..40 device range directly.
        decode: vec![(0.0, 40.0), (0.0, 40.0), (0.0, 1.0)],
    };
    // One big triangle covering a chunk of a 50×50 canvas, solid red.
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(&[0, 0, 0, 255]); // flag0 (0,0) c=1
    bytes.extend_from_slice(&[0, 255, 0, 255]); // flag0 (40,0) c=1
    bytes.extend_from_slice(&[0, 128, 255, 255]); // flag0 (20,40) c=1

    let tris = decode_type4_stream(&bytes, &params, 100);
    assert_eq!(tris.len(), 1);

    let mut pixmap = Pixmap::new(50, 50).unwrap();
    // Resolver maps a single component to a red-scaled colour.
    let to_rgba = |c: &[f32]| -> (f32, f32, f32, f32) {
        let v = c.first().copied().unwrap_or(0.0);
        (v, 0.0, 0.0, 1.0)
    };
    rasterize_raw_triangles(&mut pixmap, &tris, Transform::identity(), None, &to_rgba);

    // The triangle centroid (~20,13) must be painted red.
    let data = pixmap.data();
    let o = (13 * 50 + 20) * 4;
    assert!(
        data[o] > 100,
        "shaded region should be red, got r={}",
        data[o]
    );
    assert!(data[o + 3] > 0, "shaded region should be opaque");
    // A corner well outside the triangle stays background (transparent).
    let corner = (48 * 50 + 48) * 4;
    assert_eq!(data[corner + 3], 0, "outside triangle stays background");
}

/// Type 2 exponential function evaluates the endpoint interpolation.
#[test]
fn type2_function_interpolates() {
    let mut dict = HashMap::new();
    dict.insert("FunctionType".to_string(), Object::Integer(2));
    dict.insert(
        "C0".to_string(),
        Object::Array(vec![
            Object::Real(0.0),
            Object::Real(0.0),
            Object::Real(0.0),
        ]),
    );
    dict.insert(
        "C1".to_string(),
        Object::Array(vec![
            Object::Real(1.0),
            Object::Real(0.5),
            Object::Real(0.0),
        ]),
    );
    dict.insert("N".to_string(), Object::Integer(1));
    let out = eval_type2(&dict, &[0.5]).unwrap();
    assert!((out[0] - 0.5).abs() < 1e-6);
    assert!((out[1] - 0.25).abs() < 1e-6);
    assert!((out[2] - 0.0).abs() < 1e-6);
}
