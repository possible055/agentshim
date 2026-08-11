//! Per-page compositing sidecar for transparency + spot-ink rendering.
//!
//! ISO 32000-1:2008 §11.4 (and §11.4 in ISO 32000-2:2020) defines
//! transparency compositing as a *source-space* operation: each paint
//! is blended against the backdrop in the page-group blend space, and
//! only after every transparency / soft-mask / knockout operation has
//! been resolved does the output get handed off to the device. For a
//! press-target output the blend space is `DeviceCMYK` (or calibrated
//! CMYK via an `ICCBased` profile) and the final hand-off goes to
//! per-plate separations — that is the "composite-then-separate"
//! workflow §11.7.3 / §11.7.4 describe.
//!
//! The page renderer keeps a 4-channel `DeviceCMYK` plane alongside
//! the visible RGBA pixmap so the compose-first and overprint helpers
//! can read the backdrop CMYK quadruple directly instead of inverting
//! the post-ICC RGB (which is lossy under non-linear OutputIntent
//! profiles). This sidecar IS the §11.4 compositing buffer for the
//! process channels.
//!
//! # Spot inks
//!
//! ISO 32000-1 §11.3.4 enumerates the legal blend colour spaces
//! (`DeviceGray`, `DeviceRGB`, `DeviceCMYK`, CIE-based equivalents,
//! and bidirectional `ICCBased` of those) and explicitly excludes
//! `Separation` and `DeviceN`:
//!
//! > "The blending colour space shall be consulted only for process
//! > colours. … such colours shall not be converted to a blending
//! > colour space … the specified colour components shall be blended
//! > individually with the corresponding components of the backdrop."
//!
//! §11.6.6 (Table 147 `/CS` entry) carries the same restriction
//! forward for transparency-group colour spaces. §11.7.3 prescribes
//! the sidecar model:
//!
//! > "When an object is painted transparently with a spot colour
//! > component that is available in the output device, that colour
//! > shall be composited with the corresponding spot colour
//! > component of the backdrop, independently of the compositing that
//! > is performed for process colours. A spot colour retains its own
//! > identity; it shall not be subject to conversion to or from the
//! > colour space of the enclosing transparency group or page."
//!
//! Concretely: the spot lanes ride *alongside* the process blend
//! space, not inside it. They are per-component buffers that the
//! compositing math touches separately from the process lanes.
//!
//! # §11.7.4.2 blend-mode split
//!
//! §11.7.4.2 is the dispositive rule for non-separable and
//! non-white-preserving blend modes on spot channels:
//!
//! > "The PDF graphics state specifies only one current blend mode
//! > parameter, which shall always apply to process colorants and
//! > sometimes to spot colorants as well. Specifically, only
//! > separable, white-preserving blend modes shall be used for spot
//! > colours. If the specified blend mode is not separable and
//! > white-preserving, it shall apply only to process colour
//! > components, and the **Normal** blend mode shall be substituted
//! > for spot colours."
//!
//! The four non-separable modes (`/Hue`, `/Saturation`, `/Color`,
//! `/Luminosity`, §11.3.5.3) AND the two separable-but-non-white-
//! preserving modes (`/Difference`, `/Exclusion`, §11.3.5.2 Note 2)
//! all trigger `/Normal` substitution on spot lanes. This is encoded
//! by [`BlendModeClass`](crate::rendering::sidecar::BlendModeClass)
//! below.
//!
//! Process lanes always honour the requested blend mode; for non-sep
//! modes the §11.3.5.3 CMYK projection (complement `CMY → RGB`,
//! blend, complement back; `K = K_b` for Hue / Saturation / Color and
//! `K = K_s` for Luminosity) applies. That math lives in the renderer
//! (round 2 will wire it for the spot-aware paths); this module
//! supplies only the classification helper.
//!
//! # Storage layout
//!
//! The `CmykSidecar` storage type (crate-private; see the type
//! definition below) owns two separate buffers:
//!
//! - `cmyk`: a packed `4·w·h` byte plane with the four `DeviceCMYK`
//!   channels in `(C, M, Y, K)` order, row-major, top-left origin.
//!   This matches the round-4 layout exactly so every existing
//!   process-plane helper (mirror, compose-first, overprint) consumes
//!   it unchanged.
//! - `spots`: a plane-per-ink stack. For `N` discovered spot inks the
//!   buffer is `N·w·h` bytes long; spot `i`'s plane is the slice
//!   `spots[i·w·h .. (i+1)·w·h]`. Each byte is a tint value (0 = no
//!   ink, 255 = full tint) per the §8.6.6 model and §11.7.3
//!   "additive value of 1.0 (or subtractive tint value of 0.0)"
//!   resting-state rule.
//!
//! Spot names live in `spot_names`, ordered as `get_page_inks_deep`
//! returns them (sorted ASCII, deduped, with `/All` and `/None`
//! filtered out per §8.6.6.4).

use std::collections::HashMap;
use std::sync::Arc;

use crate::document::PdfDocument;
use crate::object::Object;

mod detection;
mod extraction;
mod initial;

pub(crate) use detection::{
    discover_page_spot_inks, is_recognised_mode, page_declares_transparency,
    page_declares_transparency_or_overprint, separable_blend,
};
use extraction::process_names_if_valid_prefix;
pub(crate) use extraction::{extract_paint_spot_inks, extract_process_paint_cmyk};
pub(crate) use initial::{initial_colour_for_space, InitialColour};

/// Classification of a PDF blend-mode name into the three categories
/// §11.7.4.2 cares about.
///
/// Used by the compositor to decide whether the spot lanes should
/// honour the requested blend mode or substitute `/Normal`. Process
/// lanes always honour the requested mode regardless of class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlendModeClass {
    /// Separable AND white-preserving. ISO 32000-1 §11.3.5.2: the
    /// ten standard modes whose formula reduces to the source colour
    /// when the backdrop is white. Spot lanes apply the requested
    /// mode component-wise.
    ///
    /// Members: `/Normal`, `/Multiply`, `/Screen`, `/Overlay`,
    /// `/Darken`, `/Lighten`, `/ColorDodge`, `/ColorBurn`,
    /// `/HardLight`, `/SoftLight`.
    SeparableWhitePreserving,
    /// Separable but NOT white-preserving. ISO 32000-1 §11.3.5.2
    /// Note 2 names exactly two: `/Difference` and `/Exclusion`.
    /// Spot lanes substitute `/Normal` per §11.7.4.2.
    SeparableNonWhitePreserving,
    /// Non-separable. ISO 32000-1 §11.3.5.3 lists exactly four:
    /// `/Hue`, `/Saturation`, `/Color`, `/Luminosity`. Their formulas
    /// project to 3-component RGB; on a CMYK blend space the CMY
    /// channels run through the projection and the K channel follows
    /// the §11.3.5.3 rule (backdrop K for Hue/Saturation/Color,
    /// source K for Luminosity). Spot lanes substitute `/Normal` per
    /// §11.7.4.2.
    NonSeparable,
}

/// Process-lane dispatch under §11.7.4.2. The rule is one-line: the
/// process lanes always honour the requested blend mode. The enum
/// exists so the call site reads as "process_dispatch == UseRequested"
/// (single variant today) and round 2's wiring can match on it without
/// magic booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessBlendDispatch {
    /// Run the requested PDF blend mode on the process lanes. For
    /// separable modes this is component-wise per §11.3.5.2; for
    /// non-separable modes this is the §11.3.5.3 RGB-projection with
    /// the K-channel rule for CMYK blend spaces.
    UseRequested,
}

/// Spot-lane dispatch under §11.7.4.2. Either "apply the requested
/// blend mode component-wise" (only when the BM is separable AND
/// white-preserving) or "substitute `/Normal`" (every other class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotBlendDispatch {
    /// Apply the requested blend mode to spot lanes component-wise.
    /// Reachable only when the BM is separable AND white-preserving.
    UseRequested,
    /// Substitute `/Normal` (source-over) on spot lanes regardless of
    /// the requested blend mode. The §11.7.4.2 rule: non-separable
    /// AND non-white-preserving modes have no defensible spot-lane
    /// behaviour, so the conforming reader paints spots as if the
    /// graphics state declared `/BM /Normal`.
    SubstituteNormal,
}

impl BlendModeClass {
    /// Classify a PDF blend-mode name into one of the three §11.7.4.2
    /// categories.
    ///
    /// Per ISO 32000-1 §11.6.3, an unknown blend mode name shall fall
    /// back to `/Normal`. We honour that by classifying unknown names
    /// as [`BlendModeClass::SeparableWhitePreserving`] — the same
    /// class `/Normal` itself belongs to. This matches the existing
    /// `pdf_blend_mode_to_skia` fallback in `src/rendering/mod.rs`.
    pub fn from_name(name: &str) -> Self {
        match name {
            // ISO 32000-1 §11.3.5.2: ten separable modes; all
            // white-preserving except Difference and Exclusion (Note 2).
            "Normal" | "Multiply" | "Screen" | "Overlay" | "Darken" | "Lighten" | "ColorDodge"
            | "ColorBurn" | "HardLight" | "SoftLight" => Self::SeparableWhitePreserving,
            "Difference" | "Exclusion" => Self::SeparableNonWhitePreserving,
            // ISO 32000-1 §11.3.5.3: four non-separable modes.
            "Hue" | "Saturation" | "Color" | "Luminosity" => Self::NonSeparable,
            // §11.6.3 fallback: unknown names render as /Normal.
            _ => Self::SeparableWhitePreserving,
        }
    }

    /// Process-lane dispatch decision. Always
    /// [`ProcessBlendDispatch::UseRequested`] per §11.7.4.2: "the
    /// current blend mode parameter … shall always apply to process
    /// colorants".
    pub fn process_dispatch(&self) -> ProcessBlendDispatch {
        ProcessBlendDispatch::UseRequested
    }

    /// Spot-lane dispatch decision per §11.7.4.2.
    pub fn spot_dispatch(&self) -> SpotBlendDispatch {
        match self {
            Self::SeparableWhitePreserving => SpotBlendDispatch::UseRequested,
            Self::SeparableNonWhitePreserving | Self::NonSeparable => {
                SpotBlendDispatch::SubstituteNormal
            }
        }
    }
}

// `spot_names` and the spot tint planes are populated by the
// discovery pre-pass at page setup; the per-paint operator writes
// land in round 2. Round 1 only exposes them through the
// `test-support` feature accessors on `PageRenderer`, so without
// `test-support` the fields and the readers are dead.
//
// We allow `dead_code` on the impl rather than `#[cfg(feature = ...)]`
// on each method because round 2 will wire these into the renderer's
// hot path unconditionally; gating them on `test-support` now would
// just be churn to undo.
#[allow(dead_code)]
/// Per-page CMYK + spot-ink compositing sidecar.
///
/// Allocated once at the top of [`super::PageRenderer::render_page_with_options`]
/// when the page declares a CMYK `OutputIntent` and any
/// transparency / overprint trigger. The sidecar lives until the page
/// finishes rendering, then is dropped.
///
/// The CMYK plane is the §11.4 compositing buffer for the four
/// process channels (`DeviceCMYK` blend space). The spot planes are
/// the §11.7.3 sidecar — one byte per pixel per ink, blended
/// independently of the process channels.
///
/// Round 1 introduces the spot-plane storage and the page-level
/// discovery pre-pass; round 2 will wire per-paint-op writes from
/// `Separation` / `DeviceN` paint operators into the spot lanes.
#[derive(Debug)]
pub(crate) struct CmykSidecar {
    /// Pixmap dimensions `(width, height)`. Captured at allocation
    /// time and used for spot-plane indexing.
    dims: (u32, u32),
    /// Packed 4-byte-per-pixel `DeviceCMYK` plane in `(C, M, Y, K)`
    /// order, row-major, top-left origin. Length is `4 · w · h`.
    /// This is the round-4 layout preserved byte-for-byte so every
    /// existing process-lane helper continues to work unchanged.
    cmyk: Vec<u8>,
    /// Ordered names of every discovered spot ink. Order matches the
    /// `spots` plane stack: `spot_names[i]` is the colorant name of
    /// the plane at `spots[i·w·h .. (i+1)·w·h]`. Populated by the
    /// pre-pass via [`PdfDocument::get_page_inks_deep`] which sorts
    /// ASCII and dedups; `/All` and `/None` are filtered out by that
    /// helper per §8.6.6.4.
    spot_names: Vec<String>,
    /// Stack of per-ink tint planes. Length is `spot_names.len() · w
    /// · h`. Plane `i` lives at `spots[i·w·h .. (i+1)·w·h]`, one byte
    /// per pixel (0 = no ink, 255 = full tint). Initialised to zero
    /// per §11.7.3 ("an additive value of 1.0 or a subtractive tint
    /// value of 0.0 shall be assumed" for an unset component).
    spots: Vec<u8>,
}

#[allow(dead_code)]
impl CmykSidecar {
    /// Allocate the sidecar for a page of `(width, height)` pixels
    /// and the given set of spot ink names.
    ///
    /// The CMYK plane and every spot plane initialise to zero — the
    /// §11.7.3 subtractive resting state. The caller is responsible
    /// for driving the per-paint mirrors that update both the CMYK
    /// and spot lanes as the content stream renders.
    pub(crate) fn new(width: u32, height: u32, spot_names: Vec<String>) -> Self {
        let pixels = (width as usize) * (height as usize);
        let cmyk = vec![0u8; 4 * pixels];
        let spots = vec![0u8; spot_names.len() * pixels];
        Self {
            dims: (width, height),
            cmyk,
            spot_names,
            spots,
        }
    }

    /// Pixmap dimensions in `(width, height)` order.
    pub(crate) fn dims(&self) -> (u32, u32) {
        self.dims
    }

    /// Read-only slice over the packed `(C, M, Y, K)` plane.
    pub(crate) fn cmyk(&self) -> &[u8] {
        &self.cmyk
    }

    /// Mutable slice over the packed `(C, M, Y, K)` plane.
    pub(crate) fn cmyk_mut(&mut self) -> &mut [u8] {
        &mut self.cmyk
    }

    /// Ordered list of spot ink names. Empty when the page declares
    /// no `Separation` / non-process `DeviceN` colorants.
    pub(crate) fn spot_names(&self) -> &[String] {
        &self.spot_names
    }

    /// Read-only slice over the tint plane for spot ink `index`.
    /// Returns `None` when `index >= spot_count()`.
    pub(crate) fn spot_plane(&self, index: usize) -> Option<&[u8]> {
        let (w, h) = self.dims;
        let plane_size = (w as usize) * (h as usize);
        let start = index.checked_mul(plane_size)?;
        let end = start.checked_add(plane_size)?;
        if end > self.spots.len() {
            return None;
        }
        Some(&self.spots[start..end])
    }

    /// Mutable slice over the tint plane for spot ink `index`.
    /// Returns `None` when `index >= spot_count()`. The per-paint spot
    /// mirror writes through this accessor to compose new tints
    /// against the backdrop.
    pub(crate) fn spot_plane_mut(&mut self, index: usize) -> Option<&mut [u8]> {
        let (w, h) = self.dims;
        let plane_size = (w as usize) * (h as usize);
        let start = index.checked_mul(plane_size)?;
        let end = start.checked_add(plane_size)?;
        if end > self.spots.len() {
            return None;
        }
        Some(&mut self.spots[start..end])
    }

    /// Find the spot plane index for an ink name, or `None` when the
    /// name was not discovered on the page (the device has no plate
    /// for it per §8.6.6.3 — the composite path's alternate colour
    /// space then provides the approximation on the visible pixmap).
    pub(crate) fn spot_index(&self, ink: &str) -> Option<usize> {
        self.spot_names.iter().position(|n| n == ink)
    }

    /// Read-only view of every spot plane stacked end-to-end. Layout
    /// matches the internal `spots` buffer: plane `i` lives at
    /// `[i·w·h, (i+1)·w·h)`. Used by the SMask path to snapshot every
    /// spot lane before the paint mirror writes so the post-paint
    /// attenuation can blend `m·post + (1-m)·pre` per pixel per lane.
    pub(crate) fn spots_all(&self) -> &[u8] {
        &self.spots
    }

    /// Mutable counterpart of [`Self::spots_all`]. The SMask attenuation
    /// path writes the per-lane blend back through this slice.
    pub(crate) fn spots_all_mut(&mut self) -> &mut [u8] {
        &mut self.spots
    }

    /// Decompose one of the four `DeviceCMYK` process plates from the
    /// packed interleaved sidecar plane.
    ///
    /// ISO 32000-1 §10.5 (separated plate output) prescribes one
    /// grayscale plate per ink whose pixel value equals the subtractive
    /// tint of that ink at that pixel (0 = no ink, 255 = full tint).
    /// The composite-then-separate workflow §11.7.3 + §11.7.4.2 mandate
    /// arrives at the §10.5 plate by running the §11.4 compositing in
    /// the process blend space first, then extracting per-ink lanes
    /// from the composited buffer.
    ///
    /// `ink` is matched case-sensitively against the four process
    /// colorant names "Cyan" / "Magenta" / "Yellow" / "Black". Any
    /// other name returns `None`; spot inks go through
    /// [`Self::spot_plate`].
    ///
    /// Returns a fresh `Vec<u8>` (length `w · h`) because the storage
    /// layout interleaves the four process channels — the requested
    /// channel's pixels are not contiguous in memory and a slice cannot
    /// describe them. Callers wrap the buffer in their own per-plate
    /// surface type and the allocation cost is one pass over `4 · w · h`
    /// bytes regardless.
    pub(crate) fn process_plate(&self, ink: &str) -> Option<Vec<u8>> {
        let channel: usize = match ink {
            "Cyan" => 0,
            "Magenta" => 1,
            "Yellow" => 2,
            "Black" => 3,
            _ => return None,
        };
        let (w, h) = self.dims;
        let pixels = (w as usize) * (h as usize);
        let mut out = Vec::with_capacity(pixels);
        for px in 0..pixels {
            out.push(self.cmyk[px * 4 + channel]);
        }
        Some(out)
    }

    /// Borrow the spot tint plane for a named spot ink, or `None` when
    /// the ink was not in the active spot set surfaced by
    /// [`discover_page_spot_inks`].
    ///
    /// ISO 32000-1 §8.6.6.3: a `Separation` / `DeviceN` colorant for
    /// which the device has no plate falls back to the alternate
    /// colour-space approximation on the visible composite; the
    /// per-plate output (§10.5) drops the colorant. Returning `None`
    /// here lets the separation entry point allocate an all-zero plate
    /// per the spec's "no plate" semantic.
    ///
    /// Returns a borrowed slice (no allocation) because each spot
    /// plane is stored as a contiguous `w · h` byte block — see the
    /// layout note on [`Self`].
    pub(crate) fn spot_plate(&self, ink: &str) -> Option<&[u8]> {
        let idx = self.spot_index(ink)?;
        self.spot_plane(idx)
    }

    /// Overwrite the packed `(C, M, Y, K)` plane with `data`. Used by
    /// the knockout-group cumulative replay path to restore the
    /// group's initial backdrop state before composing each element so
    /// later paints compose against the backdrop rather than the
    /// accumulated paint from earlier elements
    /// (ISO 32000-1 §11.4.6.2).
    ///
    /// Panics if `data.len() != self.cmyk.len()`. The caller is the
    /// knockout-group replay which snapshots the exact buffer before
    /// the loop.
    pub(crate) fn restore_cmyk(&mut self, data: &[u8]) {
        debug_assert_eq!(data.len(), self.cmyk.len());
        self.cmyk.copy_from_slice(data);
    }

    /// Overwrite the spot plane stack with `data`. Companion to
    /// [`Self::restore_cmyk`] for the spot lanes inside a knockout
    /// group's cumulative replay. ISO 32000-1 §11.3.3 + §11.4.6.2:
    /// "a single shape value and opacity value shall be maintained at
    /// each point in the computed group results; they shall apply to
    /// both process and spot colour components" — so the knockout's
    /// "compose against backdrop" rule covers the spot lanes too,
    /// which means each replay iteration must start from the group's
    /// backdrop spot state, not the previously-composed state.
    ///
    /// Panics if `data.len() != self.spots.len()`.
    pub(crate) fn restore_spots(&mut self, data: &[u8]) {
        debug_assert_eq!(data.len(), self.spots.len());
        self.spots.copy_from_slice(data);
    }
}

#[cfg(test)]
mod tests;
