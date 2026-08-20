mod profile;
mod request;
mod result;

#[cfg(feature = "bench-internals")]
pub use profile::{GlobStageTimings, ProfiledGlob};
pub use request::GlobEntryType;
pub use request::GlobError;
pub use request::GlobRequest;
#[cfg(feature = "bench-internals")]
pub use request::execute_profiled_with_traversal;
#[cfg(any(test, feature = "bench-internals"))]
pub use request::{GlobTraversal, execute, execute_with_traversal};
pub(crate) use request::{execute_output_with_budget, memory_charge};

#[cfg(test)]
use request::{MAX_MATCHES, PATH_OMISSION};
#[cfg(test)]
use result::{BoundedCollector, GlobMatch, render, render_with_budget};

#[cfg(test)]
#[path = "glob/tests.rs"]
mod test_suite;
