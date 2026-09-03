mod cursor;
mod fingerprint;
mod hooks;
mod office;
mod pdf;
mod prepared;
mod request;
mod text;

#[cfg(test)]
use crate::encoding::DecodeError;
pub use fingerprint::FileFingerprint;
#[cfg(feature = "bench-internals")]
pub use fingerprint::{FingerprintMetrics, fingerprint_metrics, reset_fingerprint_metrics};
pub(crate) use prepared::{
    Attempt, DocumentMemoryBudgets, PreparedRead, execute_prepared_with_budget, prepare,
};
pub use request::PdfMode;
#[cfg(any(test, feature = "bench-internals"))]
pub use request::execute;
#[cfg(test)]
pub use request::execute_output;
pub use request::{ReadError, ReadRequest};

pub(crate) use hooks::run_forced_pdf_block;
#[cfg(test)]
use hooks::{AFTER_READ_HOOK, BEFORE_READ_HOOK};
#[cfg(any(test, feature = "test-hooks"))]
pub use hooks::{
    FORCED_CHANGES, FORCED_PDF_BLOCK_MS, FORCED_PDF_RUNTIME_LIMIT, global_read_state_guard,
};
#[cfg(test)]
use pdf::MAX_IMAGE_BASE64_BYTES;
#[cfg(test)]
use request::{MAX_LINE_COUNT, TEXT_READ_MEMORY_BYTES};

#[cfg(test)]
#[path = "read/tests/mod.rs"]
mod test_suite;
#[cfg(any(test, feature = "test-hooks"))]
pub use test_support::*;
#[cfg(any(test, feature = "test-hooks"))]
mod test_support;
