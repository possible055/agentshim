use std::io;

pub use crate::path::{FileAccess, ReadScope, RepositoryRoot, ResolvedPath};

#[derive(Clone, Copy, Debug)]
pub enum OpenReadStrategy {
    Individual,
    SameParentBatch,
}

pub fn open_read_batches(
    access: &FileAccess,
    paths: &[ResolvedPath],
    batch_size: usize,
    strategy: OpenReadStrategy,
) -> io::Result<usize> {
    if batch_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "batch size must be positive",
        ));
    }
    let mut opened_count = 0_usize;
    for batch in paths.chunks(batch_size) {
        let opened = match strategy {
            OpenReadStrategy::Individual => batch
                .iter()
                .map(|path| access.open_read(path))
                .collect::<io::Result<Vec<_>>>()?,
            OpenReadStrategy::SameParentBatch => access
                .open_read_same_parent_batch(batch)?
                .into_iter()
                .collect::<io::Result<Vec<_>>>()?,
        };
        opened_count = opened_count.saturating_add(opened.len());
    }
    Ok(opened_count)
}

pub mod glob {
    pub use crate::tools::glob::{
        GlobRequest, GlobStageTimings, GlobTraversal, ProfiledGlob, execute,
        execute_profiled_with_traversal, execute_with_traversal,
    };
}

pub mod grep {
    pub use crate::tools::grep::{
        GrepBenchmarkVariant, GrepMode, GrepRequest, GrepSourcePolicy, GrepStageTimings,
        GrepTraversal, GrepWorkerMetrics, PathnameReopenPolicy, ProfiledGrep, execute,
        execute_profiled, execute_profiled_with_traversal, execute_profiled_with_variant,
        execute_with_traversal, execute_with_variant, reset_worker_metrics, worker_metrics,
    };
}

pub mod read {
    pub use crate::tools::read::{
        FingerprintMetrics, ReadRequest, execute, fingerprint_metrics, reset_fingerprint_metrics,
    };
}
