//! Structured warning surface.
//!
//! `PdfDocument::structured_warnings()` returns the warnings raised since
//! the document was opened, as a list of structured `Warning` records.
//! Callers who want diagnostics as data (rather than stderr text from
//! `log::warn!`) opt in to this surface. The existing `log::warn!`
//! calls continue to fire so the `setup_logging(level="WARNING")`
//! shape keeps working.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    sync::{Arc, Mutex},
};

/// A single structured warning raised during PDF processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Warning {
    /// The category — used by callers to filter.
    pub category: WarningCategory,
    /// The page index the warning was raised on, if any. `None` means
    /// the warning is document-scoped (xref recovery, trailer parse,
    /// etc.).
    pub page: Option<usize>,
    /// Free-form message. Matches the `log::warn!` strings to
    /// preserve grep-ability for users transitioning off the stderr
    /// noise.
    pub message: String,
    /// PDF spec section the warning references, when applicable.
    /// E.g. "7.3.8.1" for the stream-keyword newline violation.
    pub spec_section: Option<&'static str>,
}

/// Coarse-grained category for filtering. Each maps to a target in
/// `log::warn!` calls — `pdf_oxide::parser`, `pdf_oxide::fonts`,
/// `pdf_oxide::content`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningCategory {
    /// PDF spec violations during xref / stream / content-stream parsing.
    /// E.g. "SPEC VIOLATION: No newline after stream keyword".
    SpecViolation,
    /// Font has no `ToUnicode` entry; falling back to AGL / CID-as-
    /// Unicode chain.
    ToUnicodeMissing,
    /// Xref table corrupt; reconstructing from `obj`/`endobj` scan.
    XrefRecovery,
    /// Content stream exceeded `MAX_OPERATORS` cap; truncating.
    OperatorCapExceeded,
    /// Type 3 font detected — may require special glyph name mapping.
    Type3Font,
    /// Unexpected EOF while reading an object header / body.
    EofPremature,
    /// Encryption / decryption related warning.
    Encryption,
    /// Other font warnings (DescendantFonts inline-dict fallback, etc.).
    Font,
    /// Layout / reading-order warnings.
    Layout,
}

impl WarningCategory {
    /// Stable kebab-case string for cross-binding consumption.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SpecViolation => "spec_violation",
            Self::ToUnicodeMissing => "to_unicode_missing",
            Self::XrefRecovery => "xref_recovery",
            Self::OperatorCapExceeded => "operator_cap_exceeded",
            Self::Type3Font => "type3_font",
            Self::EofPremature => "eof_premature",
            Self::Encryption => "encryption",
            Self::Font => "font",
            Self::Layout => "layout",
        }
    }
}

/// Thread-safe sink for warnings raised during a single `PdfDocument`
/// lifetime. Backed by a `Mutex<Vec<Warning>>` so multi-threaded usage
/// (e.g. parallel-page extraction) doesn't lose warnings to a data race.
///
/// A scoped caller may hold it in an `Arc` so free-function producers on the
/// same worker thread can route warnings without process-global retention.
#[derive(Debug, Default)]
pub struct WarningSink {
    warnings: Mutex<Vec<Warning>>,
}

thread_local! {
    static WARNING_SCOPES: RefCell<Vec<Arc<WarningSink>>> = const { RefCell::new(Vec::new()) };
}

struct WarningScope;

impl Drop for WarningScope {
    fn drop(&mut self) {
        WARNING_SCOPES.with(|scopes| {
            scopes.borrow_mut().pop();
        });
    }
}

pub(crate) fn with_warning_sink<T>(sink: &Arc<WarningSink>, work: impl FnOnce() -> T) -> T {
    WARNING_SCOPES.with(|scopes| scopes.borrow_mut().push(Arc::clone(sink)));
    let _scope = WarningScope;
    work()
}

/// Route a warning from a free function to the document currently executing on this thread.
pub(crate) fn push_scoped_warning(warning: Warning) {
    WARNING_SCOPES.with(|scopes| {
        if let Some(sink) = scopes.borrow().last() {
            sink.push(warning);
        }
    });
}

impl WarningSink {
    /// Create an empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a new warning. Inexpensive — no `log` macro fired here; the
    /// existing `log::warn!` sites continue to fire on their own. Use
    /// `push_with_log` from the migrated call sites to emit both.
    pub fn push(&self, warning: Warning) {
        if let Ok(mut v) = self.warnings.lock() {
            v.push(warning);
        }
        // If the mutex was poisoned, silently drop — better than panic.
    }

    /// Snapshot of all warnings raised so far. Returns owned clones so
    /// the caller can keep them past the document's lifetime.
    pub fn snapshot(&self) -> Vec<Warning> {
        self.warnings.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// Total warning count.
    pub fn len(&self) -> usize {
        self.warnings.lock().map(|v| v.len()).unwrap_or(0)
    }

    /// True if no warnings have been raised.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all warnings. Used by `PdfDocument::reset_warnings()` for
    /// callers who want to track per-operation warnings.
    pub fn clear(&self) {
        if let Ok(mut v) = self.warnings.lock() {
            v.clear();
        }
    }

    /// Push multiple warnings at once. Used by callers that merge a
    /// drained external sink (e.g. the process-wide global sink) into
    /// the per-document sink under a single lock acquisition.
    pub fn extend(&self, warnings: impl IntoIterator<Item = Warning>) {
        if let Ok(mut v) = self.warnings.lock() {
            v.extend(warnings);
        }
    }

    /// Drain and return all accumulated warnings.
    pub fn take(&self) -> Vec<Warning> {
        if let Ok(mut v) = self.warnings.lock() {
            std::mem::take(&mut *v)
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_starts_empty() {
        let sink = WarningSink::new();
        assert!(sink.is_empty());
        assert_eq!(sink.len(), 0);
        assert_eq!(sink.snapshot().len(), 0);
    }

    #[test]
    fn push_and_snapshot() {
        let sink = WarningSink::new();
        sink.push(Warning {
            category: WarningCategory::ToUnicodeMissing,
            page: Some(0),
            message: "Type0 font 'X' has no ToUnicode entry!".into(),
            spec_section: Some("9.10.2"),
        });
        assert_eq!(sink.len(), 1);
        let snap = sink.snapshot();
        assert_eq!(snap[0].category, WarningCategory::ToUnicodeMissing);
        assert_eq!(snap[0].page, Some(0));
        assert!(snap[0].message.contains("ToUnicode"));
    }

    #[test]
    fn category_as_str_stable() {
        assert_eq!(WarningCategory::SpecViolation.as_str(), "spec_violation");
        assert_eq!(
            WarningCategory::ToUnicodeMissing.as_str(),
            "to_unicode_missing"
        );
        assert_eq!(
            WarningCategory::OperatorCapExceeded.as_str(),
            "operator_cap_exceeded"
        );
    }

    #[test]
    fn clear_resets() {
        let sink = WarningSink::new();
        sink.push(Warning {
            category: WarningCategory::SpecViolation,
            page: None,
            message: "x".into(),
            spec_section: None,
        });
        assert_eq!(sink.len(), 1);
        sink.clear();
        assert!(sink.is_empty());
    }

    #[test]
    fn warning_serializes_to_json() {
        let w = Warning {
            category: WarningCategory::SpecViolation,
            page: Some(0),
            message: "No newline after stream keyword".into(),
            spec_section: Some("7.3.8.1"),
        };
        let json = serde_json::to_string(&w).unwrap();
        assert!(json.contains("\"category\":\"spec_violation\""));
        assert!(json.contains("\"page\":0"));
        assert!(json.contains("\"spec_section\":\"7.3.8.1\""));
    }

    #[test]
    fn sink_thread_safe() {
        use std::sync::Arc;
        use std::thread;

        let sink = Arc::new(WarningSink::new());
        let mut handles = Vec::new();
        for i in 0..10 {
            let s = sink.clone();
            handles.push(thread::spawn(move || {
                s.push(Warning {
                    category: WarningCategory::Font,
                    page: Some(i),
                    message: format!("font warning {}", i),
                    spec_section: None,
                });
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(sink.len(), 10);
    }

    #[test]
    fn scoped_warnings_are_isolated_and_released() {
        let first = Arc::new(WarningSink::new());
        let second = Arc::new(WarningSink::new());
        let weak = Arc::downgrade(&first);

        with_warning_sink(&first, || {
            push_scoped_warning(Warning {
                category: WarningCategory::SpecViolation,
                page: None,
                message: "first".to_owned(),
                spec_section: None,
            });
            with_warning_sink(&second, || {
                push_scoped_warning(Warning {
                    category: WarningCategory::Font,
                    page: None,
                    message: "second".to_owned(),
                    spec_section: None,
                });
            });
        });

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        drop(first);
        assert!(weak.upgrade().is_none());
    }
}
