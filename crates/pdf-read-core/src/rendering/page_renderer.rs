//! Page renderer using tiny-skia.
//!
//! This module implements the core PDF rendering logic, converting
//! PDF operators into tiny-skia drawing commands.
#![allow(
    clippy::manual_div_ceil,
    clippy::field_reassign_with_default,
    clippy::collapsible_if,
    clippy::needless_borrow,
    clippy::get_first,
    clippy::if_same_then_else,
    clippy::needless_return_with_question_mark,
    clippy::ptr_arg
)]

use crate::content::graphics_state::{GraphicsState, GraphicsStateStack, Matrix};
use crate::content::operators::Operator;
use crate::content::parser::parse_content_stream;
use crate::document::PdfDocument;
use crate::error::{Error, Result};
use crate::object::{Object, ObjectRef};
use crate::rendering::ext_gstate::{parse_ext_g_state_inner, ParsedExtGState};
use crate::rendering::path_rasterizer::PathRasterizer;
use crate::rendering::resolution::{
    DeviceColor, IccTransformCache, LogicalColor, PaintIntent, PaintKind, PaintSide,
    ResolutionContext, ResolutionPipeline, ResolvedColor,
};
use crate::rendering::sidecar::{
    self as sidecar_mod, page_declares_transparency_or_overprint, CmykSidecar,
};
use crate::rendering::text_rasterizer::TextRasterizer;

use crate::fonts::FontInfo;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tiny_skia::{Color, PathBuilder, Pixmap, PixmapPaint, Transform};

mod backdrop_color;
mod clipping;
mod cmyk_compositing;
mod coverage;
mod forms_and_patterns;
mod image_helpers;
mod images;
mod logical_color;
mod operator_colors;
mod operator_execution;
mod operator_path_paint;
mod operator_text_show;
mod operator_xobjects;
mod overprint_and_smask;
mod overprint_helpers;
mod overprint_sidecar;
mod rendering_infrastructure;
mod resolution_pipeline;
mod shading;
mod sidecar_snapshots;
mod transfer_functions;
mod type3;
mod xobjects;

use backdrop_color::*;
use clipping::*;
use cmyk_compositing::*;
use coverage::*;
use forms_and_patterns::*;
use image_helpers::*;
use images::*;
use logical_color::*;
use operator_colors::*;
use operator_execution::*;
use operator_path_paint::*;
use operator_text_show::*;
use operator_xobjects::*;
use overprint_and_smask::*;
use overprint_helpers::*;
use overprint_sidecar::*;
use rendering_infrastructure::*;
use resolution_pipeline::*;
use shading::*;
use sidecar_snapshots::*;
use transfer_functions::*;
use type3::*;
use xobjects::*;

/// Which path-paint side(s) [`PageRenderer::pipeline_resolve_paint_gs`]
/// should resolve for the current operator.
///
/// Text operators (`Tj` / `TJ` / `'` / `"`) use the sibling
/// [`PageRenderer::pipeline_resolve_text_colors`] instead — it returns
/// `Option<ResolvedColors>` rather than `Option<GraphicsState>` so the
/// text rasteriser's internal `current_gs` clone (the one that advances
/// `text_matrix` per glyph or per `TJ` element) is the only
/// `GraphicsState` allocation on the text path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipelinePaintKind {
    /// `f`, `F`, `f*` — path-fill only.
    PathFill,
    /// `S` — path-stroke only.
    PathStroke,
    /// `B`, `b`, `B*`, `b*` — fill then stroke (one spliced clone covers
    /// both passes; the fill pass reads `fill_*` fields, the stroke pass
    /// reads `stroke_*` fields).
    PathFillStroke,
    /// `Do` with `/Subtype /Image` and `/ImageMask true` — stencil mask
    /// painted with the current fill colour. Behaviourally identical to
    /// [`PipelinePaintKind::PathFill`] inside the helper (one fill-side
    /// resolve, splice into `fill_color_rgb` / `fill_alpha`), but kept as
    /// a distinct variant so the call site reads as "image-mask intent"
    /// rather than "secretly a path fill" — and so a future wave that
    /// needs image-mask-specific routing (e.g. per-pixel overprint
    /// against an image mask painted with a spot colour) can branch on
    /// this without changing the path-fill arms.
    ImageMask,
}

/// Resolved RGBA colours destined for the text rasteriser, side by side.
///
/// The operator arm picks the colours from
/// [`PageRenderer::pipeline_resolve_text_colors`] and hands them to
/// `render_text` / `render_tj_array`. The rasteriser already clones the
/// `GraphicsState` to advance `text_matrix` per glyph or per `TJ`
/// element, so it splices the overrides into that clone — no
/// operator-arm-side allocation happens on the text path.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ResolvedColors {
    /// Fill RGBA, populated when `gs.render_mode` selects the fill side
    /// (Tr ∈ {0, 2, 4, 6}) and the pipeline produced an RGBA result.
    pub(crate) fill: Option<(f32, f32, f32, f32)>,
    /// Stroke RGBA, populated when `gs.render_mode` selects the stroke
    /// side (Tr ∈ {1, 2, 5, 6}) and the pipeline produced an RGBA
    /// result.
    pub(crate) stroke: Option<(f32, f32, f32, f32)>,
}

impl ResolvedColors {
    /// `true` when neither side carries an override.
    pub(crate) fn is_empty(&self) -> bool {
        self.fill.is_none() && self.stroke.is_none()
    }
}

/// Image output formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// Portable Network Graphics
    Png,
    /// Joint Photographic Experts Group
    Jpeg,
    /// Raw premultiplied RGBA8888 pixels, row-major, top-left origin.
    /// `data.len() == width * height * 4`. No encoding overhead; callers
    /// that need straight (un-premultiplied) alpha must convert themselves.
    RawRgba8,
}

/// Options for page rendering.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Resolution in dots per inch (default: 150)
    pub dpi: u32,
    /// Output image format (default: PNG)
    pub format: ImageFormat,
    /// Background color (RGBA, default: white)
    pub background: Option<[f32; 4]>,
    /// Whether to render annotations (default: true)
    pub render_annotations: bool,
    /// JPEG quality (1-100, default: 85)
    pub jpeg_quality: u8,
    /// Optional Content Group (layer) names to exclude from rendering.
    ///
    /// When a BDC operator with tag "OC" references an OCG whose /Name matches
    /// one of these entries, all graphical content within that marked content
    /// scope is suppressed (not painted). Empty means render everything.
    pub excluded_layers: HashSet<String>,
    /// Explicit float scale factor set by `render_page_fit`.
    /// When `Some`, bypasses integer-DPI quantization so fit dimensions are
    /// exact (issue #480). Not part of the public API; set via
    /// `render_page_fit` only.
    pub(crate) scale_override: Option<f32>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            dpi: 150,
            format: ImageFormat::Png,
            background: Some([1.0, 1.0, 1.0, 1.0]), // White background
            render_annotations: true,
            jpeg_quality: 85,
            excluded_layers: HashSet::new(),
            scale_override: None,
        }
    }
}

impl RenderOptions {
    /// Set a transparent background (no background fill).
    pub fn with_transparent_background(mut self) -> Self {
        self.background = None;
        self
    }
}

impl RenderOptions {
    /// Create options with specified DPI.
    pub fn with_dpi(dpi: u32) -> Self {
        Self {
            dpi,
            ..Default::default()
        }
    }

    /// Set format to JPEG with quality (clamped to 1-100).
    pub fn as_jpeg(mut self, quality: u8) -> Self {
        self.format = ImageFormat::Jpeg;
        self.jpeg_quality = quality.clamp(1, 100);
        self
    }

    /// Set format to raw premultiplied RGBA8888 (no encoding overhead).
    pub fn as_raw(mut self) -> Self {
        self.format = ImageFormat::RawRgba8;
        self
    }
}

/// A rendered page image.
pub struct RenderedImage {
    /// Raw image data
    pub data: Vec<u8>,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Format of the image data
    pub format: ImageFormat,
}

impl RenderedImage {
    /// Get the image data as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
}

/// Page renderer that converts PDF pages to raster images.
pub struct PageRenderer {
    options: RenderOptions,
    path_rasterizer: PathRasterizer,
    text_rasterizer: TextRasterizer,
    /// Font cache (name -> FontInfo) for current context
    fonts: HashMap<String, Arc<FontInfo>>,
    /// Color space cache (name -> Object) for current context
    color_spaces: HashMap<String, Object>,
    /// Snapshot of `options.excluded_layers` wrapped in an `Arc` so that every
    /// recursive `execute_operators` call holds a cheap reference instead of
    /// deep-cloning the set per nested Form XObject. Recomputed on the first
    /// access per `render_page` invocation. Stays `None` (no allocation) when
    /// the set is empty — the common case.
    excluded_layers_snapshot: Option<Arc<HashSet<String>>>,
    /// Per-page compiled qcms transform cache. The resolution
    /// pipeline borrows this through `ResolutionContext` so every
    /// CMYK paint operator within a page reuses the same compiled
    /// `Transform` for a given `(profile, intent)` pair. Cleared per
    /// page in `render_page_with_options`; lives across paint
    /// operators within the page.
    pub(crate) icc_transform_cache: IccTransformCache,
    /// Depth counter for the SMask materialisation path. Incremented
    /// on entry to [`Self::apply_smask_after_paint`] and decremented
    /// on exit. When the counter reaches [`MAX_SMASK_DEPTH`] further
    /// SMask materialisation is skipped (the paint is left
    /// unmodulated) so adversarial cyclic `/G` references do not
    /// drive unbounded recursion. ISO 32000-1:2008 does not mandate a
    /// numeric cap; 32 levels is well above any realistic nesting and
    /// keeps the stack usage bounded.
    smask_depth: u32,
    /// Per-page CMYK + spot-ink compositing sidecar. When present,
    /// every opaque CMYK paint mirrors its plate values into the
    /// CMYK lanes so the compose-first and overprint-correction
    /// paths read the backdrop CMYK quadruple directly instead of
    /// inverting the post-ICC RGB (lossy under non-linear OutputIntent
    /// profiles). The CMYK lane layout matches the RGBA pixmap:
    /// 4 bytes per pixel (C, M, Y, K), row-major, width × height —
    /// preserved byte-for-byte from the round-4 shape.
    ///
    /// The sidecar additionally carries one tint plane per discovered
    /// spot ink, sized at page setup from the page's resource tree
    /// (ISO 32000-1:2008 §8.6.6.4 / §8.6.6.5 declarations on
    /// `/Resources/ColorSpace` and nested Form XObjects). The spot
    /// lanes sit ALONGSIDE the CMYK blend space per §11.7.3 — they
    /// are NOT a blend space themselves, since §11.3.4 and §11.6.6
    /// (Table 147) forbid `Separation` and `DeviceN` as blend spaces.
    ///
    /// Lazy allocation: stays `None` for pages without an OutputIntent
    /// CMYK profile and pages whose resources declare no transparency
    /// or overprint trigger. The detection-OFF path is byte-identical
    /// to the pre-sidecar behaviour because the consuming helpers
    /// fall back to additive-clamp inversion when the sidecar is
    /// `None`.
    cmyk_sidecar: Option<CmykSidecar>,
    /// When `true`, allocate the CMYK + spot sidecar on every
    /// transparency-detected page regardless of whether the document
    /// declares a CMYK `OutputIntent`. The separation-renderer's
    /// composite-then-decompose entry point flips this so the spot
    /// lanes and the process plane survive the render even for press
    /// jobs whose `OutputIntent` is missing or non-CMYK. The detection
    /// gate ([`page_declares_transparency_or_overprint`]) is still
    /// honoured; detection-OFF pages never allocate a sidecar.
    pub(crate) force_cmyk_sidecar: bool,
    /// Latch on the H3b silent-K=0 warning: when the document declares
    /// `/OutputIntents` but no usable CMYK profile parses out, the
    /// RGB→CMYK fallback emits K=0 (losing the K plane). The first
    /// fallback hit logs once; subsequent paints stay silent so the
    /// log doesn't spam on a degenerate document. Reset on each
    /// `render_page_with_options` entry.
    k_zero_warning_emitted: bool,
    /// Recursion depth for Type 3 glyph rendering. A Type 3 glyph's
    /// CharProcs stream is executed re-entrantly through
    /// [`Self::execute_operators`]; a glyph that (directly or via a Form
    /// XObject) shows text in the same Type 3 font would otherwise recurse
    /// without bound. Incremented on entry to [`Self::render_type3_glyph`]
    /// and decremented on exit; glyphs at or beyond [`MAX_TYPE3_DEPTH`] are
    /// skipped (their advance width is still applied by the caller).
    type3_depth: u32,
    /// Active Type 3 `d1` fill-colour lock. When `Some`, a `d1` glyph
    /// description is being executed: the glyph is a stencil painted with
    /// this fill colour and every colour-setting operator inside it is
    /// ignored (ISO 32000-1:2008 §9.6.5.2). `None` for `d0` glyphs and all
    /// ordinary content, which paint with their own colour operators.
    type3_fill_lock: Option<(f32, f32, f32)>,
}

/// Maximum SMask materialisation recursion depth. A cyclic
/// `/SMask /G` chain (form XObject whose own ExtGState declares the
/// same `/SMask`) would otherwise drive unbounded recursion. The cap
/// is chosen above any realistic nesting depth so legitimate PDFs are
/// unaffected; adversarial inputs fall through to the no-soft-mask
/// branch once the cap engages.
pub(crate) const MAX_SMASK_DEPTH: u32 = 32;

/// Maximum Type 3 glyph rendering recursion depth. A Type 3 CharProcs
/// stream is executed re-entrantly, so a glyph that shows text in the same
/// Type 3 font (directly or through a nested Form XObject) would recurse
/// without bound. The cap sits well above any realistic nesting; glyphs at
/// or beyond it are skipped while their advance width is still applied.
pub(crate) const MAX_TYPE3_DEPTH: u32 = 8;

impl PageRenderer {
    /// Create a new page renderer with the specified options.
    pub fn new(options: RenderOptions) -> Self {
        Self {
            options,
            path_rasterizer: PathRasterizer::new(),
            text_rasterizer: TextRasterizer::new(),
            fonts: HashMap::new(),
            color_spaces: HashMap::new(),
            excluded_layers_snapshot: None,
            icc_transform_cache: IccTransformCache::new(),
            smask_depth: 0,
            cmyk_sidecar: None,
            force_cmyk_sidecar: false,
            k_zero_warning_emitted: false,
            type3_depth: 0,
            type3_fill_lock: None,
        }
    }

    /// Take ownership of the per-page CMYK + spot-ink sidecar produced
    /// by the most recent [`Self::render_page_with_options`] call.
    /// Leaves the renderer's slot empty so a subsequent render starts
    /// fresh.
    ///
    /// Used by the separation entry point
    /// ([`super::separation_renderer::render_separations`]) to harvest
    /// the populated process + spot lanes after a composite render and
    /// decompose them into per-plate output (ISO 32000-1 §10.5 plates,
    /// §11.7.3 spot lanes, §11.7.4.2 BM split).
    pub(crate) fn take_cmyk_sidecar(&mut self) -> Option<CmykSidecar> {
        self.cmyk_sidecar.take()
    }

    /// Number of qcms transform constructions the per-page cache has
    /// observed since the last `render_page_with_options` call. Test-
    /// support only: never enabled in production builds. Lets the
    /// integration suite assert "1000 same-colour CMYK paints built 1
    /// transform" without racing concurrent tests that might also
    /// trigger `Transform::new_srgb_target` via the global counter.
    #[cfg(feature = "test-support")]
    pub fn icc_transform_cache_build_count(&self) -> usize {
        self.icc_transform_cache.build_count()
    }

    /// Total `IccTransformCache::get_or_build` calls (hits + misses)
    /// observed since the last `render_page_with_options` call. Test-
    /// support only. Distinguishes a properly-hoisted per-paint
    /// lookup from a per-pixel regression: the cache returns a cached
    /// `Arc<Transform>` on every hit so `build_count` stays at 1
    /// either way, but the `content_hash` SipHash over the whole
    /// profile blob runs on every call, hit or miss. A correctly
    /// hoisted hot loop therefore yields lookup_count ≈ paint count;
    /// a per-pixel regression yields lookup_count proportional to
    /// painted pixels.
    #[cfg(feature = "test-support")]
    pub fn icc_transform_cache_lookup_count(&self) -> usize {
        self.icc_transform_cache.lookup_count()
    }

    /// Number of CMYK→CMYK retarget cache misses observed since the
    /// last `render_page_with_options` call. Test-support only. Pins
    /// the M2 retarget cache: a page with many DeviceN /Process
    /// /ICCBased N=4 paints under one OutputIntent must build the
    /// retarget transform exactly once per unique `(src_profile,
    /// dst_profile, intent)` tuple, not once per paint.
    #[cfg(feature = "test-support")]
    pub fn icc_transform_cache_cmyk_retarget_build_count(&self) -> usize {
        self.icc_transform_cache.cmyk_retarget_build_count()
    }

    /// Pixmap dimensions of the per-page compositing sidecar, or
    /// `None` when the sidecar was not allocated for the most recent
    /// `render_page_with_options` call (detection-OFF).
    ///
    /// Test-support only — gates round-1 spot-ink discovery probes
    /// and round-4 CMYK plane shape probes.
    #[cfg(feature = "test-support")]
    pub fn cmyk_sidecar_dims(&self) -> Option<(u32, u32)> {
        self.cmyk_sidecar.as_ref().map(CmykSidecar::dims)
    }

    /// Read-only view over the sidecar's packed `(C, M, Y, K)` plane.
    /// `None` when the sidecar is not allocated.
    #[cfg(feature = "test-support")]
    pub fn cmyk_sidecar_cmyk_bytes(&self) -> Option<&[u8]> {
        self.cmyk_sidecar.as_ref().map(CmykSidecar::cmyk)
    }

    /// Ordered list of spot ink names the discovery pre-pass surfaced
    /// for the most recent render (sorted ASCII, deduped, `/All` and
    /// `/None` filtered out per ISO 32000-1 §8.6.6.4). `None` when
    /// the sidecar is not allocated.
    #[cfg(feature = "test-support")]
    pub fn cmyk_sidecar_spot_names(&self) -> Option<&[String]> {
        self.cmyk_sidecar.as_ref().map(CmykSidecar::spot_names)
    }

    /// Read-only view over the tint plane for spot ink `index`,
    /// or `None` when the sidecar is not allocated or `index` is
    /// beyond the discovered spot set.
    #[cfg(feature = "test-support")]
    pub fn cmyk_sidecar_spot_plane(&self, index: usize) -> Option<&[u8]> {
        self.cmyk_sidecar.as_ref().and_then(|s| s.spot_plane(index))
    }

    /// Render a page to a raster image.
    pub fn render_page(&mut self, doc: &PdfDocument, page_num: usize) -> Result<RenderedImage> {
        self.render_page_with_options(page_num, doc)
    }

    /// Render a page with specific options.
    pub fn render_page_with_options(
        &mut self,
        page_num: usize,
        doc: &PdfDocument,
    ) -> Result<RenderedImage> {
        // Clear caches for new page
        self.fonts.clear();
        self.color_spaces.clear();
        // The qcms transform cache is per-page: dropping every entry
        // keeps memory bounded when the renderer is reused across many
        // pages with distinct /OutputIntents profiles, while still
        // amortising transform construction across paints within a
        // single page.
        self.icc_transform_cache.clear();
        // Reset the H3b silent-K=0 warning latch so a new page's first
        // RGB-to-CMYK fallback under a declared-but-unparseable
        // /OutputIntents profile logs once on the new page (instead
        // of staying suppressed across all subsequent renders on this
        // long-lived PageRenderer).
        self.k_zero_warning_emitted = false;

        // Refresh the excluded-layers snapshot once per page. The effective
        // set combines (a) the PDF's default-off OCGs per /OCProperties/D
        // (BaseState, /ON, /OFF) — ISO 32000-1 §8.11.4 — with (b) the caller's
        // explicit excluded_layers. This makes the renderer respect the PDF's
        // default visibility configuration, matching a viewer's initial state.
        let default_off = crate::optional_content::compute_default_off_ocgs(doc);
        let effective: HashSet<String> = default_off
            .into_iter()
            .chain(self.options.excluded_layers.iter().cloned())
            .collect();
        self.excluded_layers_snapshot = if effective.is_empty() {
            None
        } else {
            Some(Arc::new(effective))
        };

        // Get page info
        let page_info = doc.get_page_info(page_num)?;
        let media_box = page_info.media_box;

        // Calculate output dimensions, accounting for page rotation
        // `%` is a remainder and preserves sign, so a legal negative /Rotate (e.g. -90,
        // equivalent to 270 per ISO 32000-1 s7.7.3.3 Table 30) matched neither 90 nor
        // 270 below and the page rendered unrotated. rem_euclid normalizes to 0..359,
        // matching get_page_rotation's own `((raw % 360) + 360) % 360` convention.
        let rotation = page_info.rotation.rem_euclid(360);
        let (page_w, page_h) = if rotation == 90 || rotation == 270 {
            (media_box.height, media_box.width) // Swap for landscape
        } else {
            (media_box.width, media_box.height)
        };
        let scale = self
            .options
            .scale_override
            .unwrap_or(self.options.dpi as f32 / 72.0);
        let (width, height) = if self.options.scale_override.is_some() {
            // Float scale path: round to avoid off-by-one from exact fractional pixels.
            // Clamp to 1 so extreme aspect ratios never produce a 0-sized pixmap.
            (
                ((page_w * scale).round() as u32).max(1),
                ((page_h * scale).round() as u32).max(1),
            )
        } else {
            (
                (page_w * scale).ceil() as u32,
                (page_h * scale).ceil() as u32,
            )
        };

        // Create pixmap
        let mut pixmap = Pixmap::new(width, height)
            .ok_or_else(|| Error::InvalidPdf("Failed to create pixmap".to_string()))?;

        // Fill background
        if let Some(bg) = self.options.background {
            let [r, g, b, a] = bg;
            pixmap.fill(Color::from_rgba(r, g, b, a).unwrap_or(Color::WHITE));
        }

        // Create base transform: PDF coordinates to pixel coordinates
        // PDF origin is bottom-left; we flip Y and apply page rotation.
        // Per PDF spec §8.3.2.3, /Rotate specifies clockwise rotation.
        // The approach: first map PDF coords to an unrotated pixel space,
        // then rotate the entire result.
        let transform = match rotation {
            90 => {
                // 90° CW rotation: portrait PDF → landscape display
                // PDF y-up (x,y) → screen y-down: screen_x = y*s, screen_y = x*s
                Transform::from_translate(-media_box.x, -media_box.y)
                    .post_concat(Transform::from_row(0.0, scale, scale, 0.0, 0.0, 0.0))
            }
            180 => Transform::from_translate(-media_box.x, -media_box.y)
                .post_scale(-scale, scale)
                .post_translate(media_box.width * scale, 0.0),
            270 => {
                // 270° CW: PDF (x,y) → screen_x = (H - y)*s, screen_y = (W - x)*s.
                //
                // The `y` row used to be `screen_y = x*s`, which put the page's
                // TOP-LEFT corner at the top-left of the raster; under a 270° turn
                // it belongs at the BOTTOM-left. That is not merely a wrong angle -
                // it is a MIRROR: the old matrix has a POSITIVE determinant, while
                // 0°/90°/180° all have a negative one (they carry the PDF y-up →
                // raster y-down flip). Text came out reversed.
                Transform::from_translate(-media_box.x, -media_box.y).post_concat(
                    Transform::from_row(
                        0.0,
                        -scale,
                        -scale,
                        0.0,
                        media_box.height * scale,
                        media_box.width * scale,
                    ),
                )
            }
            _ => {
                // No rotation (0°)
                Transform::from_translate(-media_box.x, -media_box.y)
                    .post_scale(scale, -scale)
                    .post_translate(0.0, page_h * scale)
            }
        };

        // Get page resources
        let resources = doc.get_page_resources(page_num)?;

        // Pre-load resources (v0.3.18 synchronization)
        self.load_resources(doc, &resources)?;

        // Decide whether to allocate the CMYK + spot-ink sidecar. The
        // CMYK plane costs `4·width·height` bytes per page and mirrors
        // every opaque CMYK paint so the compose-first and overprint
        // correction helpers can read the backdrop CMYK quadruple
        // directly instead of inverting the post-ICC RGB. Each spot
        // ink adds one extra plane of `width·height` bytes.
        //
        // Allocation is gated on (a) the OutputIntent declares a
        // CMYK profile — without one, the process-side helpers would
        // not fire at all — and (b) the page resources declare
        // ExtGState entries that could drive transparency or
        // overprint, or the page's Form XObjects declare /Group dicts
        // or /SMask entries (which trigger transparency-group
        // compositing). When either condition is false the sidecar
        // stays `None` and the per-paint mirror is a no-op; the
        // detection-OFF path is byte-identical to the pre-sidecar
        // behaviour.
        //
        // The spot ink set is discovered with the same walker the
        // separation renderer's per-plate path uses (§8.6.6.4 /
        // §8.6.6.5: `/Separation` and non-process `/DeviceN`
        // colorants, with `/All` and `/None` filtered out). Sizing
        // the sidecar's spot lanes up front means subsequent paint
        // operators can blind-index by ink without re-walking the
        // resource tree.
        self.cmyk_sidecar = None;
        // ISO 32000-1 §11.7.3 + §11.7.4.2 + §10.5: the sidecar carries
        // the composite-then-separate workflow's process + spot lanes.
        // The default page-renderer path gates on the OutputIntent CMYK
        // profile because the compose-first / overprint-correction
        // helpers only fire when there is a non-trivial CMYK→RGB
        // transform to compose under. The separation entry point flips
        // `force_cmyk_sidecar` so the sidecar lives on every
        // detection-ON page regardless of OutputIntent — the per-plate
        // output is meaningful even without a press ICC profile (it is
        // the raw subtractive tint at every pixel).
        let needs_cmyk_sidecar = (self.force_cmyk_sidecar
            || doc.output_intent_cmyk_profile().is_some())
            && page_declares_transparency_or_overprint(doc, &resources);
        if needs_cmyk_sidecar {
            let spot_names = sidecar_mod::discover_page_spot_inks(doc, page_num);
            self.cmyk_sidecar = Some(CmykSidecar::new(width, height, spot_names));
        }

        // Get page content stream
        let content_data = doc.get_page_content_data(page_num)?;

        // Parse content stream
        let operators = match parse_content_stream(&content_data) {
            Ok(ops) => ops,
            Err(e) => {
                return Err(e);
            }
        };

        // Execute operators
        self.execute_operators(
            &mut pixmap,
            transform,
            &operators,
            doc,
            page_num,
            &resources,
        )?;

        // Render annotations (if requested and present)
        if self.options.render_annotations {
            self.render_annotations(&mut pixmap, transform, doc, page_num)?;
        }

        // Encode to output format
        let data = match self.options.format {
            ImageFormat::Png => encode_png(&pixmap)?,
            ImageFormat::Jpeg => self.encode_jpeg(&pixmap)?,
            ImageFormat::RawRgba8 => pixmap.data().to_vec(),
        };

        Ok(RenderedImage {
            data,
            width,
            height,
            format: self.options.format,
        })
    }

    /// Load resources (fonts, color spaces) into local cache.
    fn load_resources(&mut self, doc: &PdfDocument, resources: &Object) -> Result<()> {
        if let Object::Dictionary(res_dict) = resources {
            log::debug!("Loading resources, keys: {:?}", res_dict.keys());
            // Fonts
            if let Some(font_obj) = res_dict.get("Font") {
                log::debug!("Found Font resource");
                let font_dict_obj = doc.resolve_object(font_obj)?;
                if let Some(font_dict) = font_dict_obj.as_dict() {
                    for (name, f_obj) in font_dict {
                        match doc.get_or_load_font_for_rendering(f_obj) {
                            Ok(info) => {
                                log::debug!("Resolved font '{}': subtype={}, encoding={:?}, has_to_unicode={}, has_embedded={}",
                                    info.base_font, info.subtype, info.encoding, info.to_unicode.is_some(), info.embedded_font_data.is_some());
                                self.fonts.insert(name.clone(), info);
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to parse font '{}': {}. Text using this font may render incorrectly.",
                                    name, e
                                );
                            }
                        }
                    }
                }
            }

            // Color Spaces
            if let Some(cs_obj) = res_dict.get("ColorSpace") {
                log::debug!("Found ColorSpace resource");
                let cs_dict_obj = doc.resolve_object(cs_obj)?;
                if let Some(cs_dict) = cs_dict_obj.as_dict() {
                    for (name, o) in cs_dict {
                        if let Ok(resolved_cs) = doc.resolve_object(o) {
                            log::debug!("Resolved color space '{}': {:?}", name, resolved_cs);
                            self.color_spaces.insert(name.clone(), resolved_cs);
                        }
                    }
                }
            }

            // XObjects
            if let Some(xobj_obj) = res_dict.get("XObject") {
                let xobj_dict_obj = doc.resolve_object(xobj_obj)?;
                if let Some(xobj_dict) = xobj_dict_obj.as_dict() {
                    log::debug!("XObject dict keys: {:?}", xobj_dict.keys());
                }
            }
        }

        // Share TrueType CMaps between matching fonts (essential for CID fonts with missing ToUnicode)
        self.share_truetype_cmaps();
        Ok(())
    }

    /// Share TrueType cmap tables between fonts with matching base font names.
    fn share_truetype_cmaps(&mut self) {
        let mut base_font_to_cmap = HashMap::new();

        // First pass: collect available cmaps
        for font in self.fonts.values() {
            if let Some(cmap) = font.truetype_cmap() {
                // Get base font name without subset prefix (e.g. ABCDEF+Arial -> Arial)
                let base_name = if let Some(plus_idx) = font.base_font.find('+') {
                    &font.base_font[plus_idx + 1..]
                } else {
                    &font.base_font
                };
                base_font_to_cmap.insert(base_name.to_string(), cmap.clone());
            }
        }

        // Second pass: apply cmaps to fonts missing them
        for font in self.fonts.values() {
            if font.subtype == "Type0" && font.truetype_cmap().is_none() {
                let base_name = if let Some(plus_idx) = font.base_font.find('+') {
                    &font.base_font[plus_idx + 1..]
                } else {
                    &font.base_font
                };
                if let Some(shared_cmap) = base_font_to_cmap.get(base_name) {
                    font.truetype_cmap.set(Some(shared_cmap.clone())).ok();
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "page_renderer/tests/mod.rs"]
mod tests;
