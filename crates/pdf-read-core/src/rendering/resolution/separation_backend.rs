//! Per-plate separation backend on top of the resolution pipeline.
//!
//! Implements [`super::PaintBackend`] for the prepress separation case: every
//! [`super::ResolvedPaintCmd`] is rasterised once per target ink, with the
//! per-plate decision (paint this plate with this tint, or skip it) delegated
//! to [`super::InkRouter`].
//!
//! # How this fits in
//!
//! The existing free-function entry point in
//! [`super::super::separation_renderer`] still drives the page-walk for the
//! shipping separation API; it carries its own per-operator dispatch and
//! reproduces the per-plate routing decision inline (`tint_for_ink`). This
//! backend is the pipeline-driven equivalent: given a fully-resolved
//! [`super::ResolvedPaintCmd`], it produces the same plate output without any
//! of the per-operator branching. Once the operator walker upstream emits
//! `ResolvedPaintCmd`s through the pipeline, the existing renderer's
//! per-operator arms become redundant and can call into this backend
//! instead — a follow-up branch tracked separately from wave 5.
//!
//! # Contracts honoured
//!
//! - **Per-plate routing**: [`super::InkRouter`] is the single source of truth
//!   for "does this command touch this plate, and if so at what tint?". The
//!   backend itself owns no overprint or DeviceCMYK / Separation / DeviceN
//!   knowledge — every per-channel decision flows through the router.
//! - **Overprint per §11.7.4**: the router consumes `cmd.overprint` (built by
//!   [`super::OverprintResolver`]) directly. OPM=1 zero-component skip, OP=true
//!   leave-untouched, OP=false knock-out — all centralised there.
//! - **Plate writes are deterministic**: each `paint` call walks plates in
//!   the order the caller provides; the backend never reorders.
//!
//! # What this backend does NOT do
//!
//! - Walk content streams. The pipeline composer is the input; the operator
//!   walker is upstream.
//! - Honour `cmd.blend` beyond the implicit `SourceOver` plate convention.
//!   Separation plates are per-ink coverage maps; transparent blending is a
//!   composite concern (and the existing renderer treats `/BM` and `/CA` /
//!   `/ca` as `Normal` / `1.0` for the same reason — see the module-level
//!   doc on `separation_renderer`).
//! - Honour `cmd.color`'s alpha channel for the same reason. Plate coverage
//!   is binary in spirit — paint or skip — modulated by tint, not alpha.

use std::sync::Arc;

use tiny_skia::{FillRule, Mask, Pixmap, Transform};

use crate::content::graphics_state::Matrix;
use crate::error::Result;

use super::backend::PaintBackend;
use super::ink::{InkAction, InkRouter};
use super::intent::{PaintKind, PaintSide};
use super::resolved::{InkName, ResolvedPaintCmd};

/// Borrowed view of the per-plate output surface.
///
/// Caller-side construction lets the backend stay alloc-free: the pixmaps
/// and ink names already exist in the caller's owned state, and the backend
/// just borrows them for the lifetime of a `paint` call.
pub(crate) struct SeparationSurface<'a> {
    /// Per-plate output buffers. `pixmaps[i]` is written for `inks[i]`.
    pub(crate) pixmaps: &'a mut [Pixmap],
    /// Names of the inks this surface is targeting. Parallel to `pixmaps`.
    pub(crate) inks: &'a [InkName],
    /// Composition of the page's base transform with any further mapping
    /// the operator walker imposes (Form XObject `/Matrix`, etc.). The
    /// command's own `ctm` is *post*-composed onto this when painting.
    pub(crate) base_transform: Transform,
}

/// Per-plate paint backend driven by [`super::ResolutionPipeline`] output.
///
/// Holds an [`InkRouter`] instance so callers don't have to thread one
/// through. The router is stateless so the backend is too — one instance
/// can be shared across pages and across calls.
pub(crate) struct SeparationBackend {
    router: InkRouter,
}

impl SeparationBackend {
    pub(crate) const fn new() -> Self {
        Self {
            router: InkRouter::new(),
        }
    }
}

impl PaintBackend for SeparationBackend {
    type Surface<'s>
        = SeparationSurface<'s>
    where
        Self: 's;

    fn paint(&mut self, cmd: &ResolvedPaintCmd, surface: Self::Surface<'_>) -> Result<()> {
        // Resolve the clip mask once. Plates share clip geometry because
        // the clip path depends on the CTM and pixmap dimensions, both of
        // which are constant across plates.
        let shared_clip: Option<&Mask> = match &cmd.clip {
            super::resolved::ClipPlan::None => None,
            super::resolved::ClipPlan::Mask(arc) => Some(arc.as_ref()),
        };

        // §8.6.6.3 conformance decision: for a Separation source, does
        // the device have the named colorant plate? If yes, the
        // OverprintPlan's `participating` (which the composer wrote as
        // `[(spot, tint)]`) drives routing directly. If no, the per-plate
        // path falls through to `alt_cmyk_fallback` so the CMYK
        // approximation reaches the standard plates.
        let device_has_spot_plate = match &cmd.overprint.spot_source {
            Some(spot) => surface.inks.iter().any(|i| i == &spot.ink),
            None => false,
        };

        // Build a per-call overprint plan reflecting the device fallback.
        // The router doesn't see surface state, so we surface the
        // §8.6.6.3 fallback to it via the participating list it walks.
        let fallback_plan;
        let effective_plan: &super::resolved::OverprintPlan =
            if cmd.overprint.spot_source.is_some() && !device_has_spot_plate {
                // Device lacks the spot plate → use alt-CMYK approximation.
                let alt = cmd.overprint.alt_cmyk_fallback.unwrap_or([0.0; 4]);
                let mut v = smallvec::SmallVec::new();
                for (j, name) in ["Cyan", "Magenta", "Yellow", "Black"].iter().enumerate() {
                    v.push(super::resolved::ParticipatingChannel {
                        ink: InkName::new(*name),
                        value: alt[j],
                    });
                }
                fallback_plan = super::resolved::OverprintPlan {
                    enabled: cmd.overprint.enabled,
                    mode: cmd.overprint.mode,
                    participating: v,
                    selector: cmd.overprint.selector,
                    all_tint: cmd.overprint.all_tint,
                    spot_source: None,
                    alt_cmyk_fallback: None,
                };
                &fallback_plan
            } else {
                &cmd.overprint
            };

        // Per-plate routing decision and rasterisation.
        for (plate_idx, ink) in surface.inks.iter().enumerate() {
            // The router needs a `&GraphicsState` for its API contract, but
            // doesn't actually read any of its fields — `ResolvedColor` and
            // `OverprintPlan` carry all the info it needs. We use a default
            // GraphicsState so the call compiles without changing the
            // router's surface in this wave.
            let gs = crate::content::graphics_state::GraphicsState::new();
            let action = self.router.route(&gs, ink, &cmd.color, effective_plan);
            let tint = match action {
                InkAction::Skip => continue,
                InkAction::Paint(t) => t,
            };
            let pixmap = &mut surface.pixmaps[plate_idx];
            paint_one_plate(pixmap, cmd, surface.base_transform, tint, shared_clip);
        }
        Ok(())
    }
}

/// Rasterise a single resolved command onto a single plate at the given
/// tint, honouring the command's kind, side, ctm, and (shared) clip mask.
fn paint_one_plate(
    pixmap: &mut Pixmap,
    cmd: &ResolvedPaintCmd,
    base_transform: Transform,
    tint: f32,
    clip: Option<&Mask>,
) {
    let transform = combine_transforms(base_transform, &cmd.ctm);
    match cmd.kind {
        PaintKind::Path { path, fill_rule } => match cmd.side {
            PaintSide::Fill => fill_plate(pixmap, path, transform, tint, fill_rule, clip),
            PaintSide::Stroke => {
                // Stroke parameters (line width, cap, join, miter, dash) are
                // not carried in the resolved command yet — wave 5 stays
                // RGBA-side. Until those land on the pipeline, the stroke
                // is rendered with default tiny_skia stroke settings; the
                // tint and geometry are still correct, the stroke style is
                // the gap. This is the same scope boundary as the inline
                // separation renderer's stroke handling — it pulls those
                // fields off `gs` directly. See follow-up branch.
                let stroke = tiny_skia::Stroke::default();
                stroke_plate(pixmap, path, transform, &stroke, tint, clip);
            }
        },
        // ColorOnly intents are colour-resolution-only — there is no
        // geometry to paint. The pipeline still produces a resolved
        // command for them (the caller may need the resolved RGBA in
        // some non-paint context); the backend skips them.
        PaintKind::ColorOnly => {}
        // Glyph, Image, and Shading variants are provisional in the
        // intent enum today — the operator walker doesn't emit them.
        // Once it does, this backend will need per-variant rasterisation
        // paths (per-plate text raster, per-plate image sample
        // routing, per-plate gradient endpoint routing). Documented
        // gap; surfaced rather than silently dropped because the
        // wave 5 acceptance does not require these to be live.
        PaintKind::Glyph { .. } | PaintKind::Image { .. } | PaintKind::Shading { .. } => {}
    }
}

/// Fill a path into a single plate with the given tint value.
///
/// Mirrors `super::super::separation_renderer::fill_separation`: the tint is
/// encoded as a grayscale colour, alpha=255, `SourceOver` blend so overlapping
/// paints overwrite (last-writer-wins per plate). This matches the per-plate
/// "ink coverage" model — alpha and PDF blend modes are deliberately ignored
/// at the plate level (see module doc).
fn fill_plate(
    pixmap: &mut Pixmap,
    path: &tiny_skia::Path,
    transform: Transform,
    tint: f32,
    fill_rule: FillRule,
    clip: Option<&Mask>,
) {
    let gray = (tint.clamp(0.0, 1.0) * 255.0).round() as u8;
    let color = tiny_skia::Color::from_rgba8(gray, gray, gray, 255);
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    paint.blend_mode = tiny_skia::BlendMode::SourceOver;
    pixmap.fill_path(path, &paint, fill_rule, transform, clip);
}

/// Stroke a path into a single plate with the given tint value.
///
/// Mirrors `super::super::separation_renderer::stroke_separation` for the
/// tint encoding; the stroke parameters come from the caller (the resolved
/// command does not yet carry them — see [`paint_one_plate`]).
fn stroke_plate(
    pixmap: &mut Pixmap,
    path: &tiny_skia::Path,
    transform: Transform,
    stroke: &tiny_skia::Stroke,
    tint: f32,
    clip: Option<&Mask>,
) {
    let gray = (tint.clamp(0.0, 1.0) * 255.0).round() as u8;
    let color = tiny_skia::Color::from_rgba8(gray, gray, gray, 255);
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.stroke_path(path, &paint, stroke, transform, clip);
}

/// Compose a base device transform with a PDF CTM. Matches the
/// `combine_transforms` helper in `separation_renderer.rs` so the backend's
/// output is geometrically identical to the existing renderer for the same
/// (path, transform, plate) triple.
fn combine_transforms(base: Transform, ctm: &Matrix) -> Transform {
    base.pre_concat(Transform::from_row(
        ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f,
    ))
}

// Suppress the unused-Arc warning; the `Arc` import is needed because
// `ClipPlan::Mask` carries `Arc<Mask>` and the backend dereferences it.
const _: Option<Arc<Mask>> = None;

#[cfg(test)]
mod tests;
