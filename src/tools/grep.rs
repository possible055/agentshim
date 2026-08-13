mod candidates;
mod file_search;
mod ordered;
mod profile;
mod request;
mod result;

#[cfg(feature = "bench-internals")]
pub use profile::{
    GrepStageTimings, GrepWorkerMetrics, ProfiledGrep, reset_worker_metrics, worker_metrics,
};
#[cfg(test)]
pub use request::CaseMode;
#[cfg(any(test, feature = "bench-internals"))]
pub use request::{
    GrepBenchmarkVariant, GrepMode, GrepSourcePolicy, GrepTraversal, PathnameReopenPolicy, execute,
    execute_with_traversal, execute_with_variant,
};
pub use request::{GrepError, GrepRequest};
pub(crate) use request::{base_memory_charge, execute_output_with_budget};
#[cfg(feature = "bench-internals")]
pub use request::{
    execute_profiled, execute_profiled_with_traversal, execute_profiled_with_variant,
};

#[cfg(test)]
use candidates::{CandidateCollection, candidate};
#[cfg(test)]
use file_search::{
    PlanSink, SearchPlan, search_file, search_file_with_hook, search_file_with_variant_hook,
};
#[cfg(test)]
use request::{CANDIDATE_SOFT_TARGET_BYTES, CandidatePolicy, PAGE_MEMORY_BYTES, build_matcher};
#[cfg(test)]
use result::{Page, render};

#[cfg(test)]
#[path = "grep/tests.rs"]
mod test_suite;
