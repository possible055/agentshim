//! Separation plate renderer.
//!
//! Renders individual ink separation plates as grayscale images where
//! pixel intensity represents the tint percentage of that ink at each point.
//! Used in prepress workflows, ink coverage analysis, and ML pipelines
//! that process packaging/label PDFs.
//!
//! # ICCBased heuristic
//!
//! When a fill color space resolves to an `ICCBased` array, this renderer
//! does **not** parse the embedded ICC profile. Instead it inspects the
//! component count of the current fill/stroke color: a 4-component
//! `ICCBased` space is treated as CMYK (component order C, M, Y, K), a
//! 3-component space is treated as RGB (skipped — no separation routing),
//! and a 1-component space is treated as Gray (skipped). This matches
//! the convention used by Adobe Illustrator and InDesign when exporting
//! to PDF/X-1a and PDF/X-4 with CMYK working spaces. PDFs that rely on
//! lab-CMYK profile interpretation for separation routing are out of
//! scope for this renderer; they are rare in prepress workflows that
//! ship separated artwork.
//!
//! # Images
//!
//! Raster image XObjects (`Do` with `Subtype /Image`) are routed into
//! separation plates per ISO 32000-1 §8.9:
//!
//! - **DeviceCMYK** and **ICCBased N=4** images: per-pixel C / M / Y / K
//!   samples route to the Cyan / Magenta / Yellow / Black plates. JPEG-
//!   encoded streams decode through
//!   `crate::extractors::images::decode_cmyk_jpeg_to_raw_cmyk` which
//!   preserves the Adobe APP14 inversion semantics so plate values are
//!   physical ink coverage (0 = no ink, 255 = full).
//! - **Separation /\<spot-ink\>**: the single sample channel routes to
//!   the named spot plate.
//! - **DeviceN [\<ink1\> \<ink2\> …]**: each sample channel routes to its
//!   named plate; the `tintTransform` function is not consulted —
//!   samples go directly to plates, which is the standard prepress
//!   per-plate routing convention.
//! - **Image masks** (`/ImageMask true`): the 1-bpc samples are a
//!   stencil through which the current non-stroking colour is painted.
//!   Per-plate routing uses the same `tint_for_ink` decision tree as
//!   vector fills, so `/All`, `/None`, and spot/process semantics match
//!   the rest of the renderer.
//! - **DeviceRGB / DeviceGray / ICCBased N∈{1,3}** images: skipped.
//!   RGB/Gray have no declared ink-coverage intent in the subtractive
//!   output model, so they neither paint nor knock out plates. Matches
//!   `tint_for_ink`'s vector handling.
//! - **JPX (JPEG 2000) image XObjects**: logged and skipped. No pure-
//!   Rust JP2 decoder is bundled.
//! - **Indexed images** (`[/Indexed …]`): expanded to RGB upstream and
//!   therefore skipped by separation routing for now. Indexed CMYK
//!   palettes would need a separate `expand_indexed_to_cmyk` path.
//!
//! ICC profiles (per-image and document `/OutputIntents`) and TRC /
//! BG / UCR functions are **not** consulted when routing image samples
//! to plates; samples are written verbatim. The plate is an absolute
//! ink-coverage measurement independent of any colour-management
//! transform.
//!
//! Spot / DeviceN ink *declarations* in nested Form XObject `/Resources`
//! are surfaced as plates via
//! [`crate::document::PdfDocument::get_page_inks_deep`] even when the
//! form's local content stream doesn't paint them.
//!
//! # Limitations
//!
//! The following classes of content are recognised by the operator
//! walker but not actually painted into the plate:
//!
//! - **Shading patterns** (`sh` operator) — gradients used as fills.
//! - **Tiling and shading patterns** invoked via `scn` / `SCN` with a
//!   `/Pattern` colour space.
//! - **Inline images** (`BI` / `ID` / `EI`) — prepress artwork uses
//!   XObjects exclusively.
//! - **Page annotations.** [`render_separations`] renders only the
//!   page's content stream; annotation appearance streams are not
//!   walked, in contrast to [`super::page_renderer`] which composites
//!   annotation appearances on top of the page.
//!
//! These are intentional v1 omissions: the primary use case is
//! vector and image-based prepress artwork (dielines, varnish layers,
//! spot-PMS text and shapes, CMYK photographs, spot-ink-tinted images).
//!
//! # Transparency
//!
//! Plate output is opaque: the renderer treats `fill_alpha` / `stroke_alpha`
//! from ExtGState (`/CA`, `/ca`) and the blend mode (`/BM`) as if both were
//! `1.0` / `Normal`. This is intentional — a separation plate represents ink
//! coverage on the press, not transparent compositing. Callers who need the
//! transparent intent (e.g. a 50%-alpha spot text overlay) should evaluate it
//! against the underlying content with [`super::page_renderer`] first.
//!
//! # Overprint
//!
//! The renderer implements the per-plate overprint model defined in
//! ISO 32000-1 §11.7.4 ("Overprint Control"). The ExtGState entries
//! `/OP` (stroke), `/op` (non-stroke), and `/OPM` (overprint mode) are
//! parsed and applied to the graphics state.
//!
//! - **Default (`OP = false`):** for every plate, the spec rule "areas
//!   of unspecified colorants are erased (painted with a tint value of
//!   0.0)" applies. A DeviceCMYK fill knocks out underlying Cyan,
//!   Magenta, Yellow, Black, *and* any spot inks within its shape; a
//!   Separation `/Pantone-185` fill knocks out underlying process and
//!   other-spot plates within its shape. This is the standard
//!   per-plate prepress convention.
//! - **`OP = true`:** plates outside the source's colorant set are left
//!   untouched. Designers use this to overlay spot inks on process
//!   backgrounds without knocking them out (the typical packaging /
//!   label authoring workflow).
//! - **`OPM = 1` (Adobe nonzero overprint):** when the source colour
//!   space is DeviceCMYK and overprint is enabled, a component value of
//!   exactly `0.0` is treated as "colorant not specified" — the
//!   matching plate is left untouched. Per §11.7.4.3, OPM applies only
//!   to DeviceCMYK sources; Separation and DeviceN content is
//!   unaffected by OPM and routes through OP/op alone.
//!
//! Overprint state participates in `q`/`Q` save/restore via the existing
//! graphics-state stack and propagates into Form XObjects per §8.10.1.
//! The decision happens in `tint_for_ink`, which returns either
//! `PaintAction::Paint(tint)` (write tint into the plate; 0.0 = knockout)
//! or `PaintAction::Skip` (leave the plate untouched). Spot/DeviceN
//! sources route to their named plates regardless of overprint, matching
//! the inherent behavior of real separation devices.
#![allow(
    clippy::field_reassign_with_default,
    clippy::ptr_arg,
    clippy::only_used_in_recursion
)]

use std::collections::HashMap;
use std::sync::Arc;

use tiny_skia::{FillRule, Mask, PathBuilder, Pixmap, Transform};

use crate::content::graphics_state::{GraphicsState, GraphicsStateStack, Matrix};
use crate::content::operators::{Operator, TextElement};
use crate::content::parser::parse_content_stream;
use crate::document::PdfDocument;
use crate::error::{Error, Result};
use crate::fonts::FontInfo;
use crate::object::Object;

use super::ext_gstate::{parse_ext_g_state_inner, ParsedExtGState};
use super::resolution::{
    InkName, PaintBackend, PaintIntent, PaintKind, PaintSide, ResolutionContext,
    ResolutionPipeline, SeparationBackend, SeparationSurface,
};
use super::text_rasterizer::TextRasterizer;
use crate::rendering::resolution::{DeviceColor, LogicalColor};
use smallvec::SmallVec;

mod color_resolution;
mod entry;
mod geometry;
mod images;
mod inks;
mod operator_walk;
mod paint_pipeline;
mod path_paint;
mod text;

use color_resolution::*;
use entry::*;
use geometry::*;
use images::*;
use inks::*;
use operator_walk::*;
use paint_pipeline::*;
use path_paint::*;
pub(crate) use text::fill_separation;
use text::*;

/// A rendered separation plate for a single ink.
///
/// The pixel convention is **ML/QC-friendly**: `value == ink coverage`.
/// 0 means no ink on paper at that pixel, 255 means full tint coverage.
/// To display the plate as black ink on white paper (prepress viewer
/// convention) invert before showing: `display = 255 - value`.
#[derive(Debug, Clone)]
pub struct SeparationPlate {
    /// Ink name (e.g., "Cyan", "PANTONE 185 C", "Dieline").
    pub ink_name: String,
    /// Grayscale pixel data, row-major, top-left origin.
    /// 0 = no ink, 255 = full tint. `data.len() == width * height`.
    pub data: Vec<u8>,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

/// Render all separation plates for a page.
///
/// Returns one [`SeparationPlate`] per ink. Process inks (Cyan, Magenta,
/// Yellow, Black) are always emitted; if the page uses no CMYK content
/// those plates will be all-zero. Spot inks are emitted only when the
/// page's resource dictionary declares a `Separation` or `DeviceN` colour
/// space that names them.
///
/// Each plate is a grayscale image where pixel intensity equals the
/// tint percentage of that ink (255 = full tint, 0 = no ink).
///
/// # Performance
///
/// The content stream is parsed **once** and the operator walk dispatches
/// paint operations to all referenced plates in parallel. Form XObjects
/// are also recursed into once per page. Unreferenced inks short-circuit
/// to an all-zero plate before any pixmap is allocated.
pub fn render_separations(
    doc: &PdfDocument,
    page_num: usize,
    dpi: u32,
) -> Result<Vec<SeparationPlate>> {
    let inks = collect_page_inks(doc, page_num)?;
    if inks.is_empty() {
        return Ok(Vec::new());
    }

    // Pre-parse the content stream once to detect which inks are actually
    // referenced. Plates for unreferenced inks short-circuit to an empty
    // pixmap and skip the per-plate operator walk entirely (O6).
    let referenced = collect_referenced_inks(doc, page_num)?;

    render_plates_for_inks(doc, page_num, dpi, &inks, &referenced)
}

/// Render a single ink separation plate for a page.
///
/// Returns a grayscale image where pixel intensity = tint percentage
/// of the named ink. If the ink is not present on the page, the plate
/// is all zeros.
///
/// This is a thin wrapper over the multi-ink path; if you need every
/// plate on a page, call [`render_separations`] instead — it walks the
/// content stream once for all inks together.
pub fn render_separation(
    doc: &PdfDocument,
    page_num: usize,
    ink_name: &str,
    dpi: u32,
) -> Result<SeparationPlate> {
    // Always walk operators for the requested ink — the per-page short-circuit
    // in [`render_separations`] is an optimisation that scans the resource
    // declarations to skip inks that are *definitely* unused. For the single-ink
    // entry point the caller has already named the ink they want, and the
    // scanner can miss inks reached via DefaultRGB/DefaultGray remapping
    // through colour operators like `rg`/`g`. Treat the named ink as referenced
    // and let the operator walk produce an honest plate.
    let inks = vec![ink_name.to_string()];
    let referenced = inks.clone();
    let mut plates = render_plates_for_inks(doc, page_num, dpi, &inks, &referenced)?;
    plates
        .pop()
        .ok_or_else(|| Error::InvalidPdf("render_separation: no plate produced".to_string()))
}

/// Resolved colour-space classification used by the separation pipeline.
#[derive(Debug, Clone)]
enum ResolvedSpace {
    Cmyk,
    Rgb,
    Gray,
    Separation(String),
    DeviceN(Vec<String>),
    /// ICCBased with a 4-component profile (treated as CMYK by heuristic).
    IccCmyk,
    /// ICCBased with 3 components (RGB).
    IccRgb,
    /// ICCBased with 1 component (Gray).
    IccGray,
    Unknown,
}

/// Per-plate routing decision for a single paint operation, after applying
/// the overprint rules of ISO 32000-1 §11.7.4.
///
/// - [`PaintAction::Paint`] writes the given tint into the plate. A tint
///   of 0.0 is the spec-default "knockout" — the existing
///   [`fill_separation`] / [`stroke_separation`] use opaque source-over,
///   so writing 0.0 erases any underlying ink at the touched pixels.
/// - [`PaintAction::Skip`] leaves the plate completely untouched. Used
///   when (a) the source colour space doesn't reference this plate and
///   overprint is enabled, or (b) the source is DeviceCMYK with OPM=1
///   and the component is exactly 0.0 (the "Adobe nonzero overprint"
///   rule, §11.7.4).
enum PaintAction {
    Paint(f32),
    Skip,
}

/// Per-render shared context (read-only) passed through the operator
/// walk and into recursive Form XObject invocations.
///
/// The set of target inks is **not** stored here; instead it is passed
/// as a separate `target_inks: &[&str]` slice alongside the `&mut [Pixmap]`
/// to [`execute_separation_operators`]. This keeps the borrow checker
/// happy: the pixmaps slice is the only `&mut` in play, while everything
/// in `SeparationContext` is `&`.
struct SeparationContext<'a> {
    doc: &'a PdfDocument,
    text_rasterizer: &'a TextRasterizer,
    fonts: &'a HashMap<String, Arc<FontInfo>>,
}

/// Color state tracked alongside the graphics state for separation rendering.
#[derive(Clone, Debug)]
struct SeparationColorState {
    fill_components: Vec<f32>,
    stroke_components: Vec<f32>,
}

impl SeparationColorState {
    fn new() -> Self {
        Self {
            fill_components: Vec::new(),
            stroke_components: Vec::new(),
        }
    }
}

/// State inherited from a calling context when recursing into a Form
/// XObject (PDF §8.10.1: a Form XObject's initial graphics state is
/// the calling context's graphics state).
struct InheritedState {
    fill_color_space: String,
    stroke_color_space: String,
    fill_color_cmyk: Option<(f32, f32, f32, f32)>,
    stroke_color_cmyk: Option<(f32, f32, f32, f32)>,
    fill_components: Vec<f32>,
    stroke_components: Vec<f32>,
    fill_overprint: bool,
    stroke_overprint: bool,
    overprint_mode: u8,
}
