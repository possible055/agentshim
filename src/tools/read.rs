mod fingerprint;
mod hooks;
mod pdf;
mod prepared;
mod request;
mod text;

#[cfg(test)]
use crate::encoding::DecodeError;
pub(crate) use fingerprint::FileFingerprint;
#[cfg(feature = "bench-internals")]
pub use fingerprint::{FingerprintMetrics, fingerprint_metrics, reset_fingerprint_metrics};
pub(crate) use prepared::{Attempt, PdfMemoryBudgets, PreparedRead, execute_prepared, prepare};
#[cfg(test)]
use request::PdfMode;
#[cfg(any(test, feature = "bench-internals"))]
pub use request::execute;
#[cfg(any(test, feature = "bench-internals"))]
pub(crate) use request::execute_output;
pub use request::{ReadError, ReadRequest};

#[cfg(test)]
use hooks::{AFTER_READ_HOOK, BEFORE_READ_HOOK};
#[cfg(test)]
pub(crate) use hooks::{FORCED_CHANGES, FORCED_PDF_RUNTIME_LIMIT, global_read_state_guard};
#[cfg(test)]
use pdf::{MAX_IMAGE_BASE64_BYTES, MAX_IMAGE_PAGES};
#[cfg(test)]
use request::{MAX_LINE_COUNT, TEXT_READ_MEMORY_BYTES};

#[cfg(test)]
#[path = "read/tests.rs"]
mod test_suite;
