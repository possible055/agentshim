use super::*;

// ===========================================================================
// Type 4 — free-form Gouraud triangles.
// ===========================================================================

/// Decode a Type 4 free-form Gouraud-triangle stream into raw triangles.
/// The per-vertex edge flag stitches successive triangles: flag 0 starts a
/// fresh triangle (its two successors complete it); flags 1/2 reuse an edge
/// of the previous triangle (§8.7.4.5.5, Table 84).
pub(super) fn decode_type4_stream(
    data: &[u8],
    p: &MeshParams,
    max_tris: usize,
) -> Vec<[RawVertex; 3]> {
    let mut reader = BitReader::new(data);
    let mut tris: Vec<[RawVertex; 3]> = Vec::new();
    // Previous triangle vertices (va, vb, vc) for shared-edge flags.
    let mut prev: Option<[RawVertex; 3]> = None;

    while reader.remaining() >= p.bits_per_flag as usize && tris.len() < max_tris {
        let flag = match reader.read_bits(p.bits_per_flag) {
            Some(f) => f,
            None => break,
        };
        let v = match p.read_vertex(&mut reader) {
            Some(v) => v,
            None => break,
        };

        let tri = if flag == 0 {
            // New triangle: read the two remaining vertices (their flags are
            // 0 per spec and are consumed and ignored).
            let _f2 = reader.read_bits(p.bits_per_flag);
            let v2 = match p.read_vertex(&mut reader) {
                Some(v) => v,
                None => break,
            };
            let _f3 = reader.read_bits(p.bits_per_flag);
            let v3 = match p.read_vertex(&mut reader) {
                Some(v) => v,
                None => break,
            };
            [v, v2, v3]
        } else {
            let prev_tri = match &prev {
                Some(t) => t,
                // A shared-edge flag with no predecessor is malformed; stop.
                None => break,
            };
            match flag {
                // Share the (vb, vc) edge of the previous triangle.
                1 => [prev_tri[1].clone(), prev_tri[2].clone(), v],
                // Share the (va, vc) edge of the previous triangle.
                2 => [prev_tri[0].clone(), prev_tri[2].clone(), v],
                _ => break,
            }
        };

        prev = Some(tri.clone());
        tris.push(tri);
    }
    tris
}

// ===========================================================================
// Type 5 — lattice-form Gouraud triangles.
// ===========================================================================

/// Decode a Type 5 lattice-form stream (no flags) into raw triangles.
/// Vertices are read row by row (`/VerticesPerRow` columns); each pair of
/// adjacent rows forms a strip of two triangles per cell (§8.7.4.5.6).
pub(super) fn decode_type5_stream(
    data: &[u8],
    p: &MeshParams,
    max_tris: usize,
) -> Vec<[RawVertex; 3]> {
    let vpr = p.vertices_per_row;
    if vpr < 2 {
        return Vec::new();
    }
    let mut reader = BitReader::new(data);
    let mut tris: Vec<[RawVertex; 3]> = Vec::new();
    let mut prev_row: Option<Vec<RawVertex>> = None;

    loop {
        // Read a full row; a short final row ends the mesh.
        let mut row = Vec::with_capacity(vpr);
        for _ in 0..vpr {
            match p.read_vertex(&mut reader) {
                Some(v) => row.push(v),
                None => break,
            }
        }
        if row.len() < vpr {
            break;
        }
        if let Some(top) = &prev_row {
            for i in 0..vpr - 1 {
                if tris.len() + 2 > max_tris {
                    return tris;
                }
                // Two triangles per lattice cell.
                tris.push([top[i].clone(), top[i + 1].clone(), row[i].clone()]);
                tris.push([top[i + 1].clone(), row[i + 1].clone(), row[i].clone()]);
            }
        }
        prev_row = Some(row);
    }
    tris
}

// ===========================================================================
// Types 6 & 7 — Coons / tensor-product patches.
// ===========================================================================

/// Decode a Type 6 (Coons) or Type 7 (tensor) patch stream. Shared-edge
/// flags reuse four boundary points and two corner colours of the previous
/// patch (§8.7.4.5.7 Table 85 / §8.7.4.5.8 Table 86).
pub(super) fn decode_patches(
    data: &[u8],
    is_tensor: bool,
    p: &MeshParams,
    max_patches: usize,
) -> Vec<Patch> {
    let mut reader = BitReader::new(data);
    let mut patches: Vec<Patch> = Vec::new();
    let mut prev: Option<Patch> = None;
    let total_points = if is_tensor { 16 } else { 12 };

    let read_point = |r: &mut BitReader| -> Option<Pt> {
        let rx = r.read_bits(p.bits_per_coord)?;
        let ry = r.read_bits(p.bits_per_coord)?;
        Some((
            decode_value(rx, p.bits_per_coord, p.decode[0].0, p.decode[0].1),
            decode_value(ry, p.bits_per_coord, p.decode[1].0, p.decode[1].1),
        ))
    };

    while reader.remaining() >= p.bits_per_flag as usize && patches.len() < max_patches {
        let flag = match reader.read_bits(p.bits_per_flag) {
            Some(f) => f,
            None => break,
        };

        let mut boundary = [(0.0f32, 0.0f32); 12];
        let mut interior = [(0.0f32, 0.0f32); 4];
        let mut colors: [Vec<f32>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];

        // Index of the first freshly-read boundary point and colour.
        let (start_pt, start_color) = if flag == 0 {
            (0usize, 0usize)
        } else {
            let prev_patch = match &prev {
                Some(pp) => pp,
                None => break,
            };
            // Reuse four boundary points and two corner colours of the
            // previous patch, selected by the shared-edge flag.
            let (pts, cols): ([usize; 4], [usize; 2]) = match flag {
                1 => ([3, 4, 5, 6], [1, 2]),
                2 => ([6, 7, 8, 9], [2, 3]),
                3 => ([9, 10, 11, 0], [3, 0]),
                _ => break,
            };
            for (dst, &src) in pts.iter().enumerate() {
                boundary[dst] = prev_patch.boundary[src];
            }
            colors[0] = prev_patch.colors[cols[0]].clone();
            colors[1] = prev_patch.colors[cols[1]].clone();
            (4usize, 2usize)
        };

        // Read the remaining boundary points.
        let mut ok = true;
        for slot in boundary.iter_mut().take(12).skip(start_pt) {
            match read_point(&mut reader) {
                Some(pt) => *slot = pt,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        // Tensor patches carry four extra interior points after the
        // boundary; Coons patches do not.
        if ok && is_tensor && total_points == 16 {
            for slot in interior.iter_mut() {
                match read_point(&mut reader) {
                    Some(pt) => *slot = pt,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
        }
        if ok {
            for slot in colors.iter_mut().take(4).skip(start_color) {
                match p.read_color(&mut reader) {
                    Some(c) => *slot = c,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok {
            break;
        }

        let patch = Patch {
            boundary,
            interior,
            colors,
        };
        // Keep a copy for the next patch's shared-edge reference.
        prev = Some(Patch {
            boundary: patch.boundary,
            interior: patch.interior,
            colors: patch.colors.clone(),
        });
        patches.push(patch);
    }
    patches
}
