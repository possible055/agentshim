//! Call-level resource budget shared by the parser, Markdown, and render paths.
//!
//! A tool-layer reservation is a scheduling claim, not a bound: it says how much a call
//! is expected to use, and nothing stops the parser from using more. This module is the
//! part that actually stops it, by checking before each expandable allocation rather
//! than measuring after.
//!
//! The budget travels as a per-call thread-local rather than a parameter. Threading it
//! through would mean changing every `StreamDecoder::decode` signature and every
//! intermediate caller, for a value that is constant across one call and unused by most
//! of them. PDF work for one call runs on one thread, so the scope is exact.

#![warn(dead_code)]
#![warn(clippy::all)]

use std::cell::RefCell;
use std::sync::Arc;

use crate::error::{Error, LimitScope, Result};

/// Peak bytes a page costs per accumulated text span.
///
/// Measured against this repository's dense-page fixtures rather than estimated: a page
/// that accumulates N spans peaks at roughly [`EXTRACTION_BASELINE_BYTES`] + N × this.
/// A [`crate::layout::TextSpan`] is itself only a few hundred bytes; the rest is the
/// copies each layout stage holds — reading order, dedup, merge, table detection — none
/// of which can be bounded individually without rewriting them.
///
/// This is the constant that turns a byte reservation into the span count that fits it,
/// so a page budget stays tied to the memory the call actually reserved.
const PEAK_BYTES_PER_SPAN: usize = 7_500;

/// Floor the extraction pipeline costs before the first span is accumulated.
const EXTRACTION_BASELINE_BYTES: usize = 12 * 1024 * 1024;

/// Peak bytes one parsed content-stream operator occupies in the operator vector.
///
/// The vector is built whole before execution on the full-parse paths, so its size is
/// the operator count times this, and it is live at the same time as the decoded stream
/// it was parsed from. Rounded up from the measured figure of about 51 bytes; rounding
/// further would start refusing pages that render comfortably inside the reservation,
/// and rendering is the fallback a page gets when its text cannot be read.
const PEAK_BYTES_PER_OPERATOR: usize = 64;

/// Share of the call a single content stream's operator vector may occupy.
const OPERATOR_SHARE: usize = 4;

/// Default call reservation for `auto` and `text`.
pub const DEFAULT_TEXT_CALL_BYTES: usize = 64 * 1024 * 1024;
/// Default call reservation for `image`.
pub const DEFAULT_IMAGE_CALL_BYTES: usize = 96 * 1024 * 1024;

const fn operators_within(call_total_bytes: usize) -> usize {
    let share = call_total_bytes / OPERATOR_SHARE;
    let derived = share / PEAK_BYTES_PER_OPERATOR;
    // However small the reservation, a stream must be allowed some operators.
    if derived < 4_096 {
        4_096
    } else {
        derived
    }
}

const fn spans_within(call_total_bytes: usize) -> usize {
    let spare = call_total_bytes.saturating_sub(EXTRACTION_BASELINE_BYTES);
    // A page must always be allowed some text, however small the reservation.
    let derived = spare / PEAK_BYTES_PER_SPAN;
    if derived < 512 {
        512
    } else {
        derived
    }
}

/// Hard ceilings for one call, chosen per mode.
///
/// These are sub-limits, not independent quotas: an operation must satisfy both its own
/// ceiling and what remains of [`Self::call_total_bytes`]. The sub-limits deliberately
/// sum to more than the total — each bounds one shape of allocation, and the total is
/// what bounds the call.
#[derive(Clone, Copy, Debug)]
pub struct PdfResourceLimits {
    /// Estimated live bytes held by the parsed object cache.
    pub object_cache_bytes: usize,
    /// Largest single decoded stream.
    pub single_stream_bytes: usize,
    /// Decoded XObject streams retained for reuse across pages.
    pub stream_cache_bytes: usize,
    /// Scratch for one page of Markdown.
    pub page_markdown_bytes: usize,
    /// Buffer available to xref reconstruction.
    pub xref_rebuild_bytes: usize,
    /// Render surface plus scratch.
    pub render_surface_bytes: usize,
    /// Everything above, drawn from one call-level pool.
    pub call_total_bytes: usize,
    /// Content stream operators parsed per call.
    pub content_operators: usize,
    /// Operators one content stream may parse into a single vector.
    pub stream_operators: usize,
    /// Objects admitted to the cache per call.
    pub cached_objects: usize,
    /// Text spans one page may accumulate before that page is refused.
    pub page_spans: usize,
}

impl PdfResourceLimits {
    /// `auto` and `text` at the default reservation.
    #[must_use]
    pub const fn text() -> Self {
        Self::text_within(DEFAULT_TEXT_CALL_BYTES)
    }

    /// `image` at the default reservation.
    #[must_use]
    pub const fn image() -> Self {
        Self::image_within(DEFAULT_IMAGE_CALL_BYTES)
    }

    /// `auto` and `text` sized to the reservation the call actually took.
    ///
    /// The tool layer charges a configurable number of bytes against the shared pool;
    /// deriving the ceilings from that same number is what keeps the charge honest.
    /// A fixed ceiling set beside a configurable charge is two numbers that can disagree.
    #[must_use]
    pub const fn text_within(call_total_bytes: usize) -> Self {
        Self {
            object_cache_bytes: call_total_bytes / 4,
            single_stream_bytes: call_total_bytes / 4,
            stream_cache_bytes: call_total_bytes / 4,
            page_markdown_bytes: call_total_bytes / 4,
            xref_rebuild_bytes: call_total_bytes / 8,
            // No rendering, so any render surface at all is a contract violation.
            render_surface_bytes: 0,
            call_total_bytes,
            content_operators: 4_000_000,
            stream_operators: operators_within(call_total_bytes),
            cached_objects: 500_000,
            page_spans: spans_within(call_total_bytes),
        }
    }

    /// `image` sized to the reservation the call actually took.
    #[must_use]
    pub const fn image_within(call_total_bytes: usize) -> Self {
        Self {
            object_cache_bytes: call_total_bytes / 4,
            // A page raster arrives as one big stream, so this ceiling is looser here.
            single_stream_bytes: call_total_bytes / 3,
            stream_cache_bytes: call_total_bytes / 4,
            // Image mode produces no Markdown; any is a contract violation.
            page_markdown_bytes: 0,
            xref_rebuild_bytes: call_total_bytes / 8,
            render_surface_bytes: call_total_bytes / 2,
            call_total_bytes,
            content_operators: 4_000_000,
            stream_operators: operators_within(call_total_bytes),
            cached_objects: 500_000,
            page_spans: spans_within(call_total_bytes),
        }
    }
}

impl Default for PdfResourceLimits {
    fn default() -> Self {
        Self::text()
    }
}

/// Stops long-running work between checkpoints.
pub type CancelSignal = Arc<dyn Fn() -> bool + Send + Sync>;

/// Live bytes per category, so the call total is a sum of what is actually held rather
/// than of what each ceiling would allow.
#[derive(Default)]
struct LiveBytes {
    object_cache: usize,
    stream_cache: usize,
    page_markdown: usize,
    render_surface: usize,
    xref_rebuild: usize,
}

impl LiveBytes {
    fn total(&self) -> usize {
        self.object_cache
            .saturating_add(self.stream_cache)
            .saturating_add(self.page_markdown)
            .saturating_add(self.render_surface)
            .saturating_add(self.xref_rebuild)
    }
}

struct BudgetState {
    limits: PdfResourceLimits,
    live: LiveBytes,
    operators: usize,
    cancel: Option<CancelSignal>,
}

thread_local! {
    static CURRENT: RefCell<Option<BudgetState>> = const { RefCell::new(None) };
}

/// Installs `limits` for the current thread until dropped.
///
/// Restores the previous budget on drop so nesting is safe, and so a test that installs
/// one cannot leak it into the next test on the same thread.
pub struct BudgetScope {
    previous: Option<BudgetState>,
}

impl Drop for BudgetScope {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            *current.borrow_mut() = self.previous.take();
        });
    }
}

/// Run the rest of this call under `limits`.
#[must_use]
pub fn enter(limits: PdfResourceLimits, cancel: Option<CancelSignal>) -> BudgetScope {
    let state = BudgetState {
        limits,
        live: LiveBytes::default(),
        operators: 0,
        cancel,
    };
    let previous = CURRENT.with(|current| current.borrow_mut().replace(state));
    BudgetScope { previous }
}

/// Limits in force, if any. Without a scope the core stays permissive so that direct
/// library use and the existing test suite behave as before.
#[must_use]
pub fn active_limits() -> Option<PdfResourceLimits> {
    CURRENT.with(|current| current.borrow().as_ref().map(|state| state.limits))
}

fn with_state<T>(edit: impl FnOnce(&mut BudgetState) -> T) -> Option<T> {
    CURRENT.with(|current| current.borrow_mut().as_mut().map(edit))
}

fn limit_error(resource: &'static str, scope: LimitScope, limit: usize, observed: usize) -> Error {
    Error::ResourceLimit {
        resource,
        scope,
        limit_bytes: limit as u64,
        observed_bytes: observed as u64,
    }
}

/// Refuse when one category's own ceiling, or the call total with that category set to
/// `candidate`, would be exceeded.
fn check_category(
    state: &BudgetState,
    resource: &'static str,
    ceiling: usize,
    candidate: usize,
    others: usize,
) -> Result<()> {
    if candidate > ceiling {
        return Err(limit_error(resource, LimitScope::Call, ceiling, candidate));
    }
    let total = others.saturating_add(candidate);
    if total > state.limits.call_total_bytes {
        return Err(limit_error(
            "pdf_call_total",
            LimitScope::Call,
            state.limits.call_total_bytes,
            total,
        ));
    }
    Ok(())
}

/// Fail before a decoder writes `bytes` into a fresh buffer.
///
/// Called with the size about to be produced, not the size already produced: a check
/// after the fact has already paid the allocation it was supposed to prevent.
pub(crate) fn check_stream_allocation(bytes: usize) -> Result<()> {
    let Some(result) = with_state(|state| {
        if bytes > state.limits.single_stream_bytes {
            return Err(limit_error(
                "pdf_single_stream",
                LimitScope::Call,
                state.limits.single_stream_bytes,
                bytes,
            ));
        }
        // A decoded stream is transient, but it is live alongside everything the call
        // already holds, so it counts against the total while it is being produced.
        let total = state.live.total().saturating_add(bytes);
        if total > state.limits.call_total_bytes {
            return Err(limit_error(
                "pdf_call_total",
                LimitScope::Call,
                state.limits.call_total_bytes,
                total,
            ));
        }
        Ok(())
    }) else {
        return Ok(());
    };
    result
}

/// The ceiling a bounded reader should stop at, or `u64::MAX` outside a scope.
#[must_use]
pub(crate) fn stream_ceiling() -> u64 {
    with_state(|state| {
        let spare = state
            .limits
            .call_total_bytes
            .saturating_sub(state.live.total());
        state.limits.single_stream_bytes.min(spare) as u64
    })
    .unwrap_or(u64::MAX)
}

/// Admit an object into the cache, or report which ceiling refused it.
pub(crate) fn check_cache_growth(live_bytes: usize, entries: usize) -> Result<()> {
    let Some(result) = with_state(|state| {
        let others = state.live.total() - state.live.object_cache;
        check_category(
            state,
            "pdf_object_cache",
            state.limits.object_cache_bytes,
            live_bytes,
            others,
        )?;
        if entries > state.limits.cached_objects {
            return Err(limit_error(
                "pdf_cached_objects",
                LimitScope::Call,
                state.limits.cached_objects,
                entries,
            ));
        }
        state.live.object_cache = live_bytes;
        Ok(())
    }) else {
        return Ok(());
    };
    result
}

/// Admit a decoded XObject stream into the reuse cache.
///
/// This cache is long-lived — it survives across pages for the whole call — so it is the
/// one stream allocation that genuinely accumulates, and the only one worth tracking
/// against the total rather than checking transiently.
pub(crate) fn check_stream_cache_growth(live_bytes: usize) -> Result<()> {
    let Some(result) = with_state(|state| {
        let others = state.live.total() - state.live.stream_cache;
        check_category(
            state,
            "pdf_stream_cache",
            state.limits.stream_cache_bytes,
            live_bytes,
            others,
        )?;
        state.live.stream_cache = live_bytes;
        Ok(())
    }) else {
        return Ok(());
    };
    result
}

/// The stream cache's remaining room, or `usize::MAX` outside a scope.
#[must_use]
pub(crate) fn stream_cache_room() -> usize {
    with_state(|state| {
        let by_category = state
            .limits
            .stream_cache_bytes
            .saturating_sub(state.live.stream_cache);
        let by_total = state
            .limits
            .call_total_bytes
            .saturating_sub(state.live.total());
        by_category.min(by_total)
    })
    .unwrap_or(usize::MAX)
}

pub(crate) fn check_operator_growth(operators: usize) -> Result<()> {
    let Some(result) = with_state(|state| {
        state.operators = state.operators.saturating_add(operators);
        if state.operators > state.limits.content_operators {
            return Err(limit_error(
                "pdf_content_operators",
                LimitScope::Call,
                state.limits.content_operators,
                state.operators,
            ));
        }
        Ok(())
    }) else {
        return Ok(());
    };
    result
}

/// How many text regions the fast pre-scan may materialise, or `usize::MAX` outside a
/// scope.
///
/// The pre-scan trades memory for speed by indexing every text region up front. That is
/// a good trade on an ordinary page and a bad one on a page with hundreds of thousands
/// of them, where the index outgrows the stream it indexes.
#[must_use]
pub(crate) fn prescan_region_ceiling() -> usize {
    with_state(|state| state.limits.stream_operators).unwrap_or(usize::MAX)
}

/// Refuse a content stream that has parsed more operators than one stream's share.
///
/// Page-scoped rather than call-scoped: an unusually operator-dense page is a reason to
/// skip that page, not to abandon the ones around it. The vector is built whole before
/// execution on the full-parse paths, so this is the check that keeps it from becoming
/// the largest allocation in the call.
pub(crate) fn check_stream_operators(operators: usize) -> Result<()> {
    let Some(result) = with_state(|state| {
        if operators > state.limits.stream_operators {
            return Err(limit_error(
                "pdf_stream_operators",
                LimitScope::Page,
                state.limits.stream_operators,
                operators,
            ));
        }
        Ok(())
    }) else {
        return Ok(());
    };
    result
}

/// Refuse a page that has accumulated more text spans than its reservation covers.
///
/// Scoped to the page, not the call: one unusually dense page among twenty is a reason
/// to report that page as unavailable, not to discard the other nineteen. The count is
/// derived from the reservation via [`PEAK_BYTES_PER_SPAN`], so it moves with the
/// configured budget instead of being a second, unrelated number.
pub(crate) fn check_page_spans(spans: usize) -> Result<()> {
    let Some(result) = with_state(|state| {
        if spans > state.limits.page_spans {
            return Err(limit_error(
                "pdf_page_spans",
                LimitScope::Page,
                state.limits.page_spans,
                spans,
            ));
        }
        Ok(())
    }) else {
        return Ok(());
    };
    result
}

pub(crate) fn check_xref_rebuild(bytes: usize) -> Result<()> {
    let Some(result) = with_state(|state| {
        let others = state.live.total() - state.live.xref_rebuild;
        check_category(
            state,
            "pdf_xref_rebuild",
            state.limits.xref_rebuild_bytes,
            bytes,
            others,
        )
    }) else {
        return Ok(());
    };
    result
}

/// One page's Markdown is held whole so a resume offset indexes a stable string; the
/// budget table reserves for exactly that, and this is what keeps the reservation true.
pub(crate) fn check_page_markdown(bytes: usize) -> Result<()> {
    let Some(result) = with_state(|state| {
        if state.limits.page_markdown_bytes == 0 {
            // Zero means "this mode produces none", not "unbounded".
            if bytes > 0 {
                return Err(limit_error("pdf_page_markdown", LimitScope::Call, 0, bytes));
            }
            return Ok(());
        }
        let others = state.live.total() - state.live.page_markdown;
        check_category(
            state,
            "pdf_page_markdown",
            state.limits.page_markdown_bytes,
            bytes,
            others,
        )
    }) else {
        return Ok(());
    };
    result
}

/// Release one page's scratch from the call total once it has been handed off.
pub(crate) fn release_page_markdown() {
    with_state(|state| state.live.page_markdown = 0);
}

/// Largest pixel count any single embedded image may declare.
///
/// A separate ceiling from the render surface: an image can be scaled down into a small
/// surface, so the surface check alone would not stop a decoder from expanding the
/// source first. Sized to cover 600 DPI A4 colour with headroom.
pub const MAX_IMAGE_PIXELS: u64 = 80_000_000;
/// Longest edge accepted, so an extreme aspect ratio cannot slip under the pixel cap and
/// still overflow a row-stride computation.
pub const MAX_IMAGE_EDGE_PIXELS: u32 = 20_000;

/// Reject an image's declared geometry before any buffer is sized from it.
pub(crate) fn check_image_dimensions(
    width: u32,
    height: u32,
    bits_per_component: u8,
) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(Error::Image(format!(
            "image declares a zero dimension ({width}x{height})"
        )));
    }
    if width > MAX_IMAGE_EDGE_PIXELS || height > MAX_IMAGE_EDGE_PIXELS {
        return Err(Error::ResourceLimit {
            resource: "pdf_image_edge_pixels",
            scope: LimitScope::Call,
            limit_bytes: u64::from(MAX_IMAGE_EDGE_PIXELS),
            observed_bytes: u64::from(width.max(height)),
        });
    }
    if !matches!(bits_per_component, 1 | 2 | 4 | 8 | 16) {
        return Err(Error::Image(format!(
            "image declares an unsupported {bits_per_component} bits per component"
        )));
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_IMAGE_PIXELS {
        return Err(Error::ResourceLimit {
            resource: "pdf_image_pixels",
            scope: LimitScope::Call,
            limit_bytes: MAX_IMAGE_PIXELS,
            observed_bytes: pixels,
        });
    }
    // Four components at the declared depth is the widest ordinary layout (CMYK); the
    // estimate is deliberately generous rather than exact, because refusing late costs
    // the allocation this check exists to prevent.
    let estimated = pixels
        .saturating_mul(u64::from(bits_per_component).div_ceil(8).max(1))
        .saturating_mul(4);
    check_decoded_image(estimated)
}

fn check_decoded_image(estimated_bytes: u64) -> Result<()> {
    let Some(result) = with_state(|state| {
        let ceiling = state.limits.single_stream_bytes as u64;
        if estimated_bytes > ceiling {
            return Err(Error::ResourceLimit {
                resource: "pdf_image_decode",
                scope: LimitScope::Call,
                limit_bytes: ceiling,
                observed_bytes: estimated_bytes,
            });
        }
        Ok(())
    }) else {
        return Ok(());
    };
    result
}

pub(crate) fn check_render_surface(bytes: usize) -> Result<()> {
    let Some(result) = with_state(|state| {
        if state.limits.render_surface_bytes == 0 {
            return Err(limit_error(
                "pdf_render_surface",
                LimitScope::Call,
                0,
                bytes,
            ));
        }
        let others = state.live.total() - state.live.render_surface;
        check_category(
            state,
            "pdf_render_surface",
            state.limits.render_surface_bytes,
            bytes,
            others,
        )?;
        state.live.render_surface = bytes;
        Ok(())
    }) else {
        return Ok(());
    };
    result
}

/// Release the render surface from the call total once the page has been encoded.
pub(crate) fn release_render_surface() {
    with_state(|state| state.live.render_surface = 0);
}

/// Stop between checkpoints when the caller has cancelled or the deadline passed.
pub(crate) fn check_cancelled() -> Result<()> {
    let cancelled = CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .and_then(|state| state.cancel.as_ref().map(|cancel| cancel()))
            .unwrap_or(false)
    });
    if cancelled {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
