use super::*;

/// Cubic Bézier point at parameter `t` over four control points.
fn bezier(p0: Pt, p1: Pt, p2: Pt, p3: Pt, t: f32) -> Pt {
    let mt = 1.0 - t;
    let b0 = mt * mt * mt;
    let b1 = 3.0 * mt * mt * t;
    let b2 = 3.0 * mt * t * t;
    let b3 = t * t * t;
    (
        b0 * p0.0 + b1 * p1.0 + b2 * p2.0 + b3 * p3.0,
        b0 * p0.1 + b1 * p1.1 + b2 * p2.1 + b3 * p3.1,
    )
}

/// Evaluate a tensor-product (Type 7) patch surface at `(s, t)`.
/// The 16 control points map onto a 4×4 grid; the surface is the bicubic
/// Bézier combination.
fn tensor_surface(b: &[Pt; 12], interior: &[Pt; 4], s: f32, t: f32) -> Pt {
    // 4×4 control grid `g[row][col]`, row → t (bottom..top), col → s.
    // Boundary ordering per §8.7.4.5.8 Figure; interior p13..p16 fill the
    // centre.
    let g: [[Pt; 4]; 4] = [
        [b[0], b[11], b[10], b[9]],             // row0 (bottom): p1,p12,p11,p10
        [b[1], interior[3], interior[2], b[8]], // row1: p2,p16,p15,p9
        [b[2], interior[0], interior[1], b[7]], // row2: p3,p13,p14,p8
        [b[3], b[4], b[5], b[6]],               // row3 (top): p4,p5,p6,p7
    ];
    let bt = bernstein(t);
    let bs = bernstein(s);
    let mut x = 0.0;
    let mut y = 0.0;
    for (r, brow) in g.iter().enumerate() {
        for (c, pt) in brow.iter().enumerate() {
            let w = bt[r] * bs[c];
            x += w * pt.0;
            y += w * pt.1;
        }
    }
    (x, y)
}

/// Cubic Bernstein basis weights at `t`.
#[inline]
fn bernstein(t: f32) -> [f32; 4] {
    let mt = 1.0 - t;
    [mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t]
}

/// Evaluate a Coons patch surface point at `(s, t)` directly from the 12
/// boundary control points (bilinearly-blended Coons formula).
fn coons_point(b: &[Pt; 12], s: f32, t: f32) -> Pt {
    let left = bezier(b[0], b[1], b[2], b[3], t);
    let right = bezier(b[9], b[8], b[7], b[6], t);
    let bottom = bezier(b[0], b[11], b[10], b[9], s);
    let top = bezier(b[3], b[4], b[5], b[6], s);
    let (p1, p4, p7, p10) = (b[0], b[3], b[6], b[9]);
    let blend = |lb: f32, rb: f32, bb: f32, tb: f32, c1: f32, c4: f32, c7: f32, c10: f32| -> f32 {
        (1.0 - t) * bb + t * tb + (1.0 - s) * lb + s * rb
            - ((1.0 - s) * (1.0 - t) * c1 + s * (1.0 - t) * c10 + (1.0 - s) * t * c4 + s * t * c7)
    };
    (
        blend(left.0, right.0, bottom.0, top.0, p1.0, p4.0, p7.0, p10.0),
        blend(left.1, right.1, bottom.1, top.1, p1.1, p4.1, p7.1, p10.1),
    )
}

/// Rasterise decoded patches by subdividing each into an adaptive N×N grid
/// of Gouraud cells. Corner colours are resolved once per patch and
/// interpolated bilinearly across the grid.
pub(super) fn render_patches(
    pixmap: &mut Pixmap,
    patches: &[Patch],
    is_tensor: bool,
    transform: Transform,
    clip_mask: Option<&Mask>,
    to_rgba: &dyn Fn(&[f32]) -> (f32, f32, f32, f32),
) {
    let (w, h) = (pixmap.width() as f32, pixmap.height() as f32);
    for patch in patches {
        // Resolve the four corner colours (RGBA) once.
        let c = [
            to_rgba(&patch.colors[0]),
            to_rgba(&patch.colors[1]),
            to_rgba(&patch.colors[2]),
            to_rgba(&patch.colors[3]),
        ];

        // Adaptive subdivision from the device-space size of the corners.
        let corners = [
            patch.boundary[0],
            patch.boundary[3],
            patch.boundary[6],
            patch.boundary[9],
        ];
        let dev: Vec<Pt> = corners.iter().map(|&p| map_pt(transform, p)).collect();
        let mut extent = 0.0f32;
        for i in 0..dev.len() {
            for j in i + 1..dev.len() {
                let d = ((dev[i].0 - dev[j].0).powi(2) + (dev[i].1 - dev[j].1).powi(2)).sqrt();
                extent = extent.max(d);
            }
        }
        // Skip patches wholly outside the canvas (cheap corner test).
        if dev
            .iter()
            .all(|p| p.0 < 0.0 || p.0 > w || p.1 < 0.0 || p.1 > h)
            && !bbox_intersects_canvas(&dev, w, h)
        {
            continue;
        }
        let n = ((extent / 16.0).ceil() as usize).clamp(1, MAX_SUBDIV);

        // Precompute grid nodes: device point + bilinear RGBA per (i, j).
        let node = |i: usize, j: usize| -> ((f32, f32), (f32, f32, f32, f32)) {
            let s = i as f32 / n as f32;
            let t = j as f32 / n as f32;
            let sp = if is_tensor {
                tensor_surface(&patch.boundary, &patch.interior, s, t)
            } else {
                coons_point(&patch.boundary, s, t)
            };
            let dp = map_pt(transform, sp);
            // Bilinear corner-colour blend: c1@(0,0) c2@(0,1) c3@(1,1) c4@(1,0).
            let col = bilerp_rgba(c[0], c[1], c[2], c[3], s, t);
            (dp, col)
        };

        for i in 0..n {
            for j in 0..n {
                let (p00, c00) = node(i, j);
                let (p10, c10) = node(i + 1, j);
                let (p01, c01) = node(i, j + 1);
                let (p11, c11) = node(i + 1, j + 1);
                fill_gouraud_triangle(pixmap, clip_mask, (p00, c00), (p10, c10), (p11, c11));
                fill_gouraud_triangle(pixmap, clip_mask, (p00, c00), (p11, c11), (p01, c01));
            }
        }
    }
}

/// Bilinear blend of four RGBA corners. `s`, `t` in `[0, 1]`; corners map
/// c1@(0,0), c2@(0,1), c3@(1,1), c4@(1,0).
fn bilerp_rgba(
    c1: (f32, f32, f32, f32),
    c2: (f32, f32, f32, f32),
    c3: (f32, f32, f32, f32),
    c4: (f32, f32, f32, f32),
    s: f32,
    t: f32,
) -> (f32, f32, f32, f32) {
    let w1 = (1.0 - s) * (1.0 - t);
    let w2 = (1.0 - s) * t;
    let w3 = s * t;
    let w4 = s * (1.0 - t);
    (
        w1 * c1.0 + w2 * c2.0 + w3 * c3.0 + w4 * c4.0,
        w1 * c1.1 + w2 * c2.1 + w3 * c3.1 + w4 * c4.1,
        w1 * c1.2 + w2 * c2.2 + w3 * c3.2 + w4 * c4.2,
        w1 * c1.3 + w2 * c2.3 + w3 * c3.3 + w4 * c4.3,
    )
}
