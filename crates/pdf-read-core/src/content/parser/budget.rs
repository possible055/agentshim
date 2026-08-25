use super::*;

/// Default maximum number of operators to parse from a single content
/// stream. Prevents pathological inputs (e.g., Isartor 6.1.12) from
/// consuming unbounded time and memory.
///
/// Callers can override via [`set_max_ops_per_stream`] to raise the
/// cap (or set `usize::MAX` for effectively unbounded — use with
/// caution on adversarial PDFs).
pub(super) const MAX_OPERATORS: usize = 1_000_000;

/// Global cap override for content-stream operator count. `0`
/// means "use [`MAX_OPERATORS`] default"; any other value is the
/// effective cap. Atomic so it's safe to set from one thread while
/// extraction runs on another (e.g. parallel-page extraction).
pub(super) static MAX_OPERATORS_OVERRIDE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Current effective operator cap. Reads the override if set; otherwise
/// returns [`MAX_OPERATORS`]. Internal hot-path helper.
#[inline]
pub(super) fn effective_max_operators() -> usize {
    let override_val = MAX_OPERATORS_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
    if override_val == 0 {
        MAX_OPERATORS
    } else {
        override_val
    }
}

/// Maximum consecutive parse errors (byte skips) before bailing out.
///
/// If we skip this many bytes without finding a valid operator, the
/// remaining data is likely junk, not a parseable content stream.
pub(super) const MAX_CONSECUTIVE_ERRORS: usize = 1024;

/// Refuse a stream that has parsed past what the call reserved for one operator vector.
///
/// Deliberately not folded into the truncation cap below it. That cap keeps what it has
/// and warns, which is right for an implementation limit and wrong for a budget: a page
/// silently shortened is indistinguishable from a page that was that short. Checked on a
/// stride so the cost stays off the per-operator path.
#[inline]
pub(super) fn check_operator_budget(count: usize) -> Result<()> {
    const STRIDE: usize = 4096;
    if count % STRIDE == 0 {
        crate::budget::check_stream_operators(count)?;
    }
    Ok(())
}

/// Emit the operator-cap-exceeded warning at the actual *effective* cap
/// (which may have been overridden via `set_max_ops_per_stream`). PDF
/// Spec Annex C documents implementation limits; the cap exists to
/// bound parser cost on adversarial inputs.
#[inline]
pub(super) fn push_operator_cap_warning() {
    let cap = effective_max_operators();
    let msg = format!("Content stream exceeded {cap} operators, truncating");
    log::warn!("{msg}");
    crate::extractors::warnings::push_scoped_warning(crate::extractors::warnings::Warning {
        category: crate::extractors::warnings::WarningCategory::OperatorCapExceeded,
        page: None,
        message: msg,
        spec_section: Some("Annex C"),
    });
}
