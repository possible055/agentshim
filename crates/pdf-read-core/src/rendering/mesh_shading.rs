//! Mesh and function-based shadings (ISO 32000-1 §8.7.4.5.5–§8.7.4.5.8
//! and §8.7.4.5.2).
//!
//! tiny-skia's gradient shaders cover only the axial (Type 2) and radial
//! (Type 3) shadings. The remaining shading dictionary types carry an
//! explicit geometry stream (Types 4–7) or a colour function evaluated
//! over a domain rectangle (Type 1); none map onto a tiny-skia shader, so
//! they are rasterised here by hand:
//!
//! - **Type 4** free-form Gouraud triangles — a bit-packed vertex stream
//!   with a per-vertex edge flag that stitches triangles together.
//! - **Type 5** lattice-form Gouraud triangles — a flag-free vertex grid
//!   of `/VerticesPerRow` columns, tessellated row by row.
//! - **Type 6** Coons patch meshes — cubic-Bézier boundary patches with
//!   four corner colours, subdivided into a Gouraud grid.
//! - **Type 7** tensor-product patch meshes — like Type 6 but with four
//!   extra interior control points (bicubic surface).
//! - **Type 1** function-based shadings — a `/Function` sampled over the
//!   `/Domain` rectangle, mapped through `/Matrix`.
//!
//! Colours read from the stream (or produced by the shading's optional
//! `/Function`) are handed back to the caller through a `resolve_color`
//! closure so they travel the same colour-space resolution path (§8.6) as
//! every other painted colour. Triangles are filled with barycentric
//! interpolation of the per-vertex RGBA; patches interpolate the four
//! corner RGBAs bilinearly across the subdivision grid.
//!
//! All stream reads are bounded (a short/malformed stream stops decoding
//! and paints what was decoded so far) and every count is capped, so a
//! hostile or corrupt shading can neither panic nor hang — it simply
//! paints less. This mirrors the pre-existing "unsupported shading →
//! leave unpainted" behaviour when nothing at all can be decoded.

use crate::document::PdfDocument;
use crate::error::Result;
use crate::object::Object;
use std::collections::HashMap;
use tiny_skia::{Mask, Pixmap, Transform};

mod decode;
mod functions;
mod patches;

use decode::{decode_patches, decode_type4_stream, decode_type5_stream};
use functions::{eval_pdf_function, eval_type2, num, pairs};
use patches::render_patches;

/// Hard cap on the number of triangles rasterised for one shading. Meshes
/// this large are almost always malformed; the cap bounds worst-case work.
const MAX_TRIANGLES: usize = 4_000_000;
/// Hard cap on Coons/tensor patches decoded for one shading.
const MAX_PATCHES: usize = 500_000;
/// Upper bound on the per-patch subdivision grid (N×N cells).
const MAX_SUBDIV: usize = 10;
/// Upper bound on the Type 1 domain sampling grid (N×N nodes).
const MAX_TYPE1_GRID: usize = 128;

/// A colour resolver: maps colour-space components (already in the
/// shading's `/ColorSpace`) to straight-alpha RGBA. Supplied by the
/// renderer so mesh colours travel the standard §8.6 resolution path.
pub(crate) type ColorResolver<'a> = dyn Fn(&[f32]) -> Option<(f32, f32, f32, f32)> + 'a;

/// Entry point invoked from the `sh`/`render_shading` dispatcher for
/// shading types tiny-skia cannot express as a gradient shader.
///
/// `shading` is the shading dictionary; `shading_obj` is the full resolved
/// object (a stream for Types 4–7, whose bytes carry the geometry).
/// `transform` maps shading space to device space. `resolve_color` maps
/// colour-space components to RGBA. Returns `Ok(())` in every non-fatal
/// case — an unsupported bit depth or malformed stream logs and paints
/// nothing rather than erroring.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_mesh_shading(
    pixmap: &mut Pixmap,
    shading: &HashMap<String, Object>,
    shading_obj: &Object,
    shading_type: i64,
    transform: Transform,
    doc: &PdfDocument,
    clip_mask: Option<&Mask>,
    resolve_color: &ColorResolver<'_>,
) -> Result<()> {
    // The optional `/Function` remaps a single parametric value carried by
    // each vertex/patch corner (or, for Type 1, the 2-D domain point) into
    // the shading colour space's components. When absent the stream carries
    // the colour-space components directly.
    let function = shading
        .get("Function")
        .and_then(|f| doc.resolve_object(f).ok());

    // Resolve a set of stream/function colour components to RGBA, routing
    // through `/Function` first when present.
    let to_rgba = |comps: &[f32]| -> (f32, f32, f32, f32) {
        let cs_comps: Vec<f32> = match &function {
            Some(f) => eval_pdf_function(f, doc, comps).unwrap_or_else(|| comps.to_vec()),
            None => comps.to_vec(),
        };
        resolve_color(&cs_comps).unwrap_or((0.0, 0.0, 0.0, 1.0))
    };

    match shading_type {
        1 => render_function_based(pixmap, shading, transform, clip_mask, &to_rgba),
        4..=7 => {
            let data = match shading_obj.decode_stream_data() {
                Ok(d) => d,
                Err(e) => {
                    log::debug!("Mesh shading type {shading_type}: stream decode failed: {e}");
                    return Ok(());
                }
            };
            let params = match MeshParams::parse(shading) {
                Some(p) => p,
                None => {
                    log::debug!("Mesh shading type {shading_type}: missing/invalid stream params");
                    return Ok(());
                }
            };
            match shading_type {
                4 => {
                    let tris = decode_type4_stream(&data, &params, MAX_TRIANGLES);
                    rasterize_raw_triangles(pixmap, &tris, transform, clip_mask, &to_rgba);
                }
                5 => {
                    let tris = decode_type5_stream(&data, &params, MAX_TRIANGLES);
                    rasterize_raw_triangles(pixmap, &tris, transform, clip_mask, &to_rgba);
                }
                6 | 7 => {
                    let is_tensor = shading_type == 7;
                    let patches = decode_patches(&data, is_tensor, &params, MAX_PATCHES);
                    render_patches(pixmap, &patches, is_tensor, transform, clip_mask, &to_rgba);
                }
                _ => unreachable!(),
            }
            Ok(())
        }
        other => {
            log::debug!("Unsupported shading type {other} in mesh renderer");
            Ok(())
        }
    }
}

// ===========================================================================
// Bit-packed stream reader (MSB-first, no per-field byte alignment).
// ===========================================================================

/// Sequential MSB-first bit reader over a byte slice. Every read is
/// bounded: once the cursor passes the end of the buffer, reads return
/// `None` and decoding stops gracefully.
struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Total bits remaining.
    fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.bit_pos)
    }

    /// Read `nbits` (0..=32) as an unsigned integer, MSB first. Returns
    /// `None` when fewer than `nbits` remain.
    fn read_bits(&mut self, nbits: u32) -> Option<u64> {
        let nbits = nbits as usize;
        if nbits == 0 {
            return Some(0);
        }
        if nbits > 32 || self.remaining() < nbits {
            return None;
        }
        let mut value: u64 = 0;
        for _ in 0..nbits {
            let byte = self.data[self.bit_pos >> 3];
            let bit = (byte >> (7 - (self.bit_pos & 7))) & 1;
            value = (value << 1) | bit as u64;
            self.bit_pos += 1;
        }
        Some(value)
    }
}

/// Map a raw `nbits`-wide unsigned integer onto `[lo, hi]` per a `/Decode`
/// pair (§8.7.4.5.5). `2^nbits - 1` is the maximum representable value.
fn decode_value(raw: u64, nbits: u32, lo: f32, hi: f32) -> f32 {
    let max = if nbits >= 64 {
        u64::MAX
    } else {
        (1u64 << nbits) - 1
    };
    if max == 0 {
        return lo;
    }
    let t = raw as f32 / max as f32;
    lo + t * (hi - lo)
}

// ===========================================================================
// Shared mesh stream parameters.
// ===========================================================================

/// Bit widths and the `/Decode` ranges shared by Types 4–7.
struct MeshParams {
    bits_per_flag: u32,
    bits_per_coord: u32,
    bits_per_comp: u32,
    /// `/VerticesPerRow` (Type 5 only; ignored otherwise).
    vertices_per_row: usize,
    /// `[x, y, c0, c1, ...]` decode ranges. `ncomps == decode.len() - 2`.
    decode: Vec<(f32, f32)>,
}

impl MeshParams {
    fn parse(shading: &HashMap<String, Object>) -> Option<Self> {
        let bits_per_coord = shading.get("BitsPerCoordinate")?.as_integer()? as u32;
        let bits_per_comp = shading.get("BitsPerComponent")?.as_integer()? as u32;
        // BitsPerFlag is absent on Type 5 lattices; default to a byte.
        let bits_per_flag = shading
            .get("BitsPerFlag")
            .and_then(|o| o.as_integer())
            .unwrap_or(8) as u32;
        if bits_per_coord == 0 || bits_per_coord > 32 || bits_per_comp == 0 || bits_per_comp > 32 {
            return None;
        }
        if bits_per_flag > 32 {
            return None;
        }
        let decode_arr = shading.get("Decode")?.as_array()?;
        let decode: Vec<(f32, f32)> = decode_arr
            .chunks_exact(2)
            .map(|c| (num(&c[0]), num(&c[1])))
            .collect();
        // Need at least the x and y ranges plus one colour component.
        if decode.len() < 3 {
            return None;
        }
        let vertices_per_row = shading
            .get("VerticesPerRow")
            .and_then(|o| o.as_integer())
            .unwrap_or(0)
            .max(0) as usize;
        Some(Self {
            bits_per_flag,
            bits_per_coord,
            bits_per_comp,
            vertices_per_row,
            decode,
        })
    }

    fn ncomps(&self) -> usize {
        self.decode.len() - 2
    }

    /// Read one `(x, y, comps)` vertex body (no flag) from the reader.
    fn read_vertex(&self, reader: &mut BitReader) -> Option<RawVertex> {
        let rx = reader.read_bits(self.bits_per_coord)?;
        let ry = reader.read_bits(self.bits_per_coord)?;
        let x = decode_value(rx, self.bits_per_coord, self.decode[0].0, self.decode[0].1);
        let y = decode_value(ry, self.bits_per_coord, self.decode[1].0, self.decode[1].1);
        let comps = self.read_color(reader)?;
        Some(RawVertex { x, y, comps })
    }

    /// Read the colour components (no coordinates) from the reader.
    fn read_color(&self, reader: &mut BitReader) -> Option<Vec<f32>> {
        let n = self.ncomps();
        let mut comps = Vec::with_capacity(n);
        for i in 0..n {
            let raw = reader.read_bits(self.bits_per_comp)?;
            let (lo, hi) = self.decode[2 + i];
            comps.push(decode_value(raw, self.bits_per_comp, lo, hi));
        }
        Some(comps)
    }
}

/// A vertex in shading space with its raw colour-space components.
#[derive(Clone, Debug)]
struct RawVertex {
    x: f32,
    y: f32,
    comps: Vec<f32>,
}

type Pt = (f32, f32);

/// A decoded patch: 12 boundary control points (`p1..p12`), 4 interior
/// points (`p13..p16`, tensor only; zeroed for Coons) and 4 corner colour
/// component arrays (`c1..c4`).
struct Patch {
    boundary: [Pt; 12],
    interior: [Pt; 4],
    colors: [Vec<f32>; 4],
}

// ===========================================================================
// Type 1 — function-based shading.
// ===========================================================================

/// Render a function-based (Type 1) shading: evaluate `/Function` over the
/// `/Domain` rectangle on an adaptive grid, map each node through `/Matrix`
/// then the shading→device transform, and fill the grid cells as Gouraud
/// quads (§8.7.4.5.2).
fn render_function_based(
    pixmap: &mut Pixmap,
    shading: &HashMap<String, Object>,
    transform: Transform,
    clip_mask: Option<&Mask>,
    to_rgba: &dyn Fn(&[f32]) -> (f32, f32, f32, f32),
) -> Result<()> {
    // Domain [x0 x1 y0 y1], default [0 1 0 1].
    let (dx0, dx1, dy0, dy1) = shading
        .get("Domain")
        .and_then(|o| o.as_array())
        .filter(|a| a.len() >= 4)
        .map(|a| (num(&a[0]), num(&a[1]), num(&a[2]), num(&a[3])))
        .unwrap_or((0.0, 1.0, 0.0, 1.0));

    // Matrix maps domain space to the shading's target coordinate space.
    let matrix = shading
        .get("Matrix")
        .and_then(|o| o.as_array())
        .filter(|a| a.len() >= 6)
        .map(|a| {
            [
                num(&a[0]),
                num(&a[1]),
                num(&a[2]),
                num(&a[3]),
                num(&a[4]),
                num(&a[5]),
            ]
        })
        .unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    // Map a domain point → device point (Matrix then shading→device CTM).
    let map_domain = |u: f32, v: f32| -> Pt {
        let sx = matrix[0] * u + matrix[2] * v + matrix[4];
        let sy = matrix[1] * u + matrix[3] * v + matrix[5];
        map_pt(transform, (sx, sy))
    };

    // Grid resolution from the device extent of the domain corners.
    let corners = [
        map_domain(dx0, dy0),
        map_domain(dx1, dy0),
        map_domain(dx1, dy1),
        map_domain(dx0, dy1),
    ];
    let mut extent = 0.0f32;
    for i in 0..corners.len() {
        for j in i + 1..corners.len() {
            let d = ((corners[i].0 - corners[j].0).powi(2) + (corners[i].1 - corners[j].1).powi(2))
                .sqrt();
            extent = extent.max(d);
        }
    }
    let n = (extent.ceil() as usize).clamp(2, MAX_TYPE1_GRID);

    // Precompute the (device point, RGBA) grid.
    let mut grid: Vec<((f32, f32), (f32, f32, f32, f32))> = Vec::with_capacity((n + 1) * (n + 1));
    for i in 0..=n {
        for j in 0..=n {
            let u = dx0 + (dx1 - dx0) * (i as f32 / n as f32);
            let v = dy0 + (dy1 - dy0) * (j as f32 / n as f32);
            // `to_rgba` runs the `/Function` on the 2-D domain point.
            let rgba = to_rgba(&[u, v]);
            grid.push((map_domain(u, v), rgba));
        }
    }
    let at = |i: usize, j: usize| grid[i * (n + 1) + j];
    for i in 0..n {
        for j in 0..n {
            let a = at(i, j);
            let b = at(i + 1, j);
            let c = at(i + 1, j + 1);
            let d = at(i, j + 1);
            fill_gouraud_triangle(pixmap, clip_mask, a, b, c);
            fill_gouraud_triangle(pixmap, clip_mask, a, c, d);
        }
    }
    Ok(())
}

// ===========================================================================
// Rasterisation.
// ===========================================================================

/// Resolve each raw triangle's per-vertex colour and rasterise it with
/// barycentric colour interpolation.
fn rasterize_raw_triangles(
    pixmap: &mut Pixmap,
    tris: &[[RawVertex; 3]],
    transform: Transform,
    clip_mask: Option<&Mask>,
    to_rgba: &dyn Fn(&[f32]) -> (f32, f32, f32, f32),
) {
    for tri in tris {
        let mut verts = [((0.0f32, 0.0f32), (0.0f32, 0.0f32, 0.0f32, 0.0f32)); 3];
        for (k, rv) in tri.iter().enumerate() {
            verts[k] = (map_pt(transform, (rv.x, rv.y)), to_rgba(&rv.comps));
        }
        fill_gouraud_triangle(pixmap, clip_mask, verts[0], verts[1], verts[2]);
    }
}

/// Map a shading-space point through the shading→device transform.
#[inline]
fn map_pt(transform: Transform, p: Pt) -> Pt {
    let mut pt = tiny_skia::Point { x: p.0, y: p.1 };
    transform.map_point(&mut pt);
    (pt.x, pt.y)
}

/// True when the device bounding box of the corner points overlaps the
/// canvas rectangle `[0, w] × [0, h]`.
fn bbox_intersects_canvas(corners: &[Pt], w: f32, h: f32) -> bool {
    let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in corners {
        minx = minx.min(x);
        miny = miny.min(y);
        maxx = maxx.max(x);
        maxy = maxy.max(y);
    }
    minx <= w && maxx >= 0.0 && miny <= h && maxy >= 0.0
}

type ColoredVertex = ((f32, f32), (f32, f32, f32, f32));

/// Barycentric-interpolated triangle fill directly into the pixmap's
/// premultiplied RGBA buffer, honouring the clip mask. Colours are
/// straight-alpha RGBA per vertex.
fn fill_gouraud_triangle(
    pixmap: &mut Pixmap,
    clip_mask: Option<&Mask>,
    v0: ColoredVertex,
    v1: ColoredVertex,
    v2: ColoredVertex,
) {
    let width = pixmap.width() as i32;
    let height = pixmap.height() as i32;
    if width == 0 || height == 0 {
        return;
    }
    let (p0, c0) = v0;
    let (p1, c1) = v1;
    let (p2, c2) = v2;

    // Device-space bounding box, clamped to the canvas.
    let minx = p0.0.min(p1.0).min(p2.0).floor().max(0.0) as i32;
    let maxx = p0.0.max(p1.0).max(p2.0).ceil().min(width as f32) as i32;
    let miny = p0.1.min(p1.1).min(p2.1).floor().max(0.0) as i32;
    let maxy = p0.1.max(p1.1).max(p2.1).ceil().min(height as f32) as i32;
    if minx >= maxx || miny >= maxy {
        return;
    }

    // Barycentric denominator; a degenerate (zero-area) triangle is skipped.
    let denom = (p1.1 - p2.1) * (p0.0 - p2.0) + (p2.0 - p1.0) * (p0.1 - p2.1);
    if denom.abs() < 1e-9 {
        return;
    }
    let inv_denom = 1.0 / denom;

    let mask_data = clip_mask.map(|m| m.data());
    let dest = pixmap.data_mut();

    for py in miny..maxy {
        for px in minx..maxx {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            let w0 = ((p1.1 - p2.1) * (fx - p2.0) + (p2.0 - p1.0) * (fy - p2.1)) * inv_denom;
            let w1 = ((p2.1 - p0.1) * (fx - p2.0) + (p0.0 - p2.0) * (fy - p2.1)) * inv_denom;
            let w2 = 1.0 - w0 - w1;
            // Small epsilon so shared edges between adjacent triangles fill.
            if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 {
                continue;
            }

            let mut a = w0 * c0.3 + w1 * c1.3 + w2 * c2.3;
            if a <= 0.0 {
                continue;
            }
            let pixel_idx = (py * width + px) as usize;
            if let Some(md) = mask_data {
                if let Some(&m) = md.get(pixel_idx) {
                    a *= m as f32 / 255.0;
                    if a <= 0.0 {
                        continue;
                    }
                }
            }
            let r = w0 * c0.0 + w1 * c1.0 + w2 * c2.0;
            let g = w0 * c0.1 + w1 * c1.1 + w2 * c2.1;
            let b = w0 * c0.2 + w1 * c1.2 + w2 * c2.2;
            blend_premul(dest, pixel_idx * 4, r, g, b, a);
        }
    }
}

/// Source-over blend of a straight-alpha colour into a premultiplied RGBA8
/// destination pixel.
#[inline]
fn blend_premul(dest: &mut [u8], off: usize, r: f32, g: f32, b: f32, a: f32) {
    if off + 3 >= dest.len() {
        return;
    }
    let a = a.clamp(0.0, 1.0);
    let sr = r.clamp(0.0, 1.0) * a;
    let sg = g.clamp(0.0, 1.0) * a;
    let sb = b.clamp(0.0, 1.0) * a;
    let inv = 1.0 - a;
    let dr = dest[off] as f32 / 255.0;
    let dg = dest[off + 1] as f32 / 255.0;
    let db = dest[off + 2] as f32 / 255.0;
    let da = dest[off + 3] as f32 / 255.0;
    dest[off] = ((sr + dr * inv) * 255.0).round().clamp(0.0, 255.0) as u8;
    dest[off + 1] = ((sg + dg * inv) * 255.0).round().clamp(0.0, 255.0) as u8;
    dest[off + 2] = ((sb + db * inv) * 255.0).round().clamp(0.0, 255.0) as u8;
    dest[off + 3] = ((a + da * inv) * 255.0).round().clamp(0.0, 255.0) as u8;
}

#[cfg(test)]
mod tests;
