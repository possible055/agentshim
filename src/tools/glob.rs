mod profile;
mod request;
mod result;

#[cfg(feature = "bench-internals")]
pub use profile::{GlobStageTimings, ProfiledGlob};
pub use request::GlobError;
#[cfg(feature = "bench-internals")]
pub use request::execute_profiled_with_traversal;
#[cfg(any(test, feature = "bench-internals"))]
pub use request::{GlobEntryType, GlobRequest};
#[cfg(any(test, feature = "bench-internals"))]
pub use request::{GlobTraversal, execute, execute_with_traversal};
pub(crate) use request::{execute_output, memory_charge};

#[cfg(test)]
use request::{MAX_MATCHES, PATH_OMISSION, record_match};
#[cfg(test)]
use result::{GlobMatch, TopK, render};

#[cfg(test)]
#[path = "glob/tests.rs"]
mod test_suite;
