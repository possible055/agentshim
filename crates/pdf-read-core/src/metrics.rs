//! Observation counters for the expandable data paths.
//!
//! These only observe; [`crate::budget`] enforces. The record points sit at the sites
//! already proven to see every allocation, which is where the budget checks attach.

#![warn(dead_code)]
#![warn(clippy::all)]

use std::cell::Cell;

/// Counters accumulated on one thread between [`measure`] boundaries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PdfReadMetrics {
    /// Total bytes produced by every stream decoder invocation.
    pub decoded_stream_bytes: u64,
    /// Largest single decoder output.
    pub peak_decoded_stream_bytes: u64,
    /// Number of decoder invocations.
    pub decoded_streams: u64,
    /// High-water mark of the estimated live object cache.
    pub peak_object_cache_bytes: u64,
    /// Objects admitted to the object cache.
    pub cached_objects: u64,
    /// Operators produced by content stream parsing.
    pub content_operators: u64,
    /// Width multiplied by height for every rasterised surface.
    pub render_pixels: u64,
    /// Encoded PNG bytes produced by rendering.
    pub png_bytes: u64,
    /// System font database loads. Process-wide and lazy, so the count is zero on every
    /// call that never rasterises — including the first one.
    pub font_database_loads: u64,
}

impl PdfReadMetrics {
    const ZERO: Self = Self {
        decoded_stream_bytes: 0,
        peak_decoded_stream_bytes: 0,
        decoded_streams: 0,
        peak_object_cache_bytes: 0,
        cached_objects: 0,
        content_operators: 0,
        render_pixels: 0,
        png_bytes: 0,
        font_database_loads: 0,
    };
}

thread_local! {
    static CURRENT: Cell<PdfReadMetrics> = const { Cell::new(PdfReadMetrics::ZERO) };
}

/// Run `body` with fresh counters and return its value alongside what it consumed.
///
/// Nesting is supported: the outer scope's counters are restored afterwards. A panic
/// inside `body` leaves the thread's counters zeroed rather than restored, which is
/// acceptable because nothing routes control flow on them.
pub fn measure<T>(body: impl FnOnce() -> T) -> (T, PdfReadMetrics) {
    let outer = CURRENT.with(|current| current.replace(PdfReadMetrics::ZERO));
    let value = body();
    let inner = CURRENT.with(|current| current.replace(outer));
    (value, inner)
}

/// Counters accumulated on this thread since the enclosing [`measure`] began.
#[must_use]
pub fn current() -> PdfReadMetrics {
    CURRENT.with(Cell::get)
}

fn update(edit: impl FnOnce(&mut PdfReadMetrics)) {
    CURRENT.with(|current| {
        let mut metrics = current.get();
        edit(&mut metrics);
        current.set(metrics);
    });
}

pub(crate) fn record_decoded_stream(bytes: usize) {
    let bytes = bytes as u64;
    update(|metrics| {
        metrics.decoded_streams = metrics.decoded_streams.saturating_add(1);
        metrics.decoded_stream_bytes = metrics.decoded_stream_bytes.saturating_add(bytes);
        metrics.peak_decoded_stream_bytes = metrics.peak_decoded_stream_bytes.max(bytes);
    });
}

pub(crate) fn record_object_cache(live_bytes: usize) {
    let live_bytes = live_bytes as u64;
    update(|metrics| {
        metrics.cached_objects = metrics.cached_objects.saturating_add(1);
        metrics.peak_object_cache_bytes = metrics.peak_object_cache_bytes.max(live_bytes);
    });
}

pub(crate) fn record_content_operators(count: usize) {
    let count = count as u64;
    update(|metrics| {
        metrics.content_operators = metrics.content_operators.saturating_add(count);
    });
}

pub(crate) fn record_font_database_load() {
    update(|metrics| {
        metrics.font_database_loads = metrics.font_database_loads.saturating_add(1);
    });
}

pub(crate) fn record_render(width: u32, height: u32, png_bytes: usize) {
    let pixels = u64::from(width) * u64::from(height);
    let png_bytes = png_bytes as u64;
    update(|metrics| {
        metrics.render_pixels = metrics.render_pixels.saturating_add(pixels);
        metrics.png_bytes = metrics.png_bytes.saturating_add(png_bytes);
    });
}
