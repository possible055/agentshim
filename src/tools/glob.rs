mod profile;
mod request;
mod result;

#[cfg(feature = "bench-internals")]
pub use profile::{GlobStageTimings, ProfiledGlob};
#[cfg(test)]
pub use request::GlobEntryType;
pub use request::GlobError;
#[cfg(any(test, feature = "bench-internals"))]
pub use request::GlobRequest;
#[cfg(feature = "bench-internals")]
pub use request::execute_profiled_with_traversal;
#[cfg(any(test, feature = "bench-internals"))]
pub use request::{GlobTraversal, execute, execute_with_traversal};
pub(crate) use request::{execute_output_with_budget, memory_charge};

#[cfg(test)]
use request::PATH_OMISSION;
#[cfg(test)]
use result::{GlobMatch, TopK, render};

#[cfg(test)]
#[path = "glob/tests.rs"]
mod test_suite;
