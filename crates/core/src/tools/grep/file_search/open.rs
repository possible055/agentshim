use std::io;

use cap_std::fs::File;

use crate::path::{FileAccess, ResolvedPath};
use crate::tools::read::FileFingerprint;

use crate::tools::grep::profile::{GrepProfiler, GrepStage};
use crate::tools::grep::request::PathnameReopenPolicy;

#[cfg(any(test, feature = "bench-internals"))]
pub(super) fn open_identity_candidate(
    access: &FileAccess,
    path: &ResolvedPath,
    profiler: &GrepProfiler,
    fingerprint_stage: GrepStage,
) -> io::Result<OpenedCandidate> {
    if path.is_ambient() && access.symlink_metadata_kind(path)?.is_symlink {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ambient candidate must not be a symbolic link",
        ));
    }
    let open_span = profiler.span(GrepStage::SearchOpenHandleWorker);
    let file = access.open_file_identity(path)?;
    drop(open_span);
    let fingerprint_span = profiler.span(fingerprint_stage);
    let fingerprint = FileFingerprint::from_file(&file)?;
    drop(fingerprint_span);
    if fingerprint.regular {
        Ok(OpenedCandidate { file, fingerprint })
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "candidate is not a regular file",
        ))
    }
}

pub struct OpenedCandidate {
    pub file: File,
    pub fingerprint: FileFingerprint,
}

pub(super) fn open_candidate(
    access: &FileAccess,
    path: &ResolvedPath,
    profiler: &GrepProfiler,
    fingerprint_stage: GrepStage,
    include_identity: bool,
) -> io::Result<OpenedCandidate> {
    if path.is_ambient() {
        let metadata_span = profiler.span(GrepStage::SearchSymlinkMetadataWorker);
        let metadata = access.symlink_metadata_kind(path);
        drop(metadata_span);
        if metadata?.is_symlink {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ambient candidate must not be a symbolic link",
            ));
        }
    }
    let open_span = profiler.span(GrepStage::SearchOpenHandleWorker);
    let file = access.open_read(path);
    drop(open_span);
    fingerprint_opened_candidate(file?, profiler, fingerprint_stage, include_identity)
}

pub fn fingerprint_opened_candidate(
    file: File,
    profiler: &GrepProfiler,
    fingerprint_stage: GrepStage,
    include_identity: bool,
) -> io::Result<OpenedCandidate> {
    let fingerprint_span = profiler.span(fingerprint_stage);
    let fingerprint = if include_identity {
        FileFingerprint::from_file(&file)?
    } else {
        FileFingerprint::from_file_state(&file)?
    };
    drop(fingerprint_span);
    if fingerprint.regular {
        Ok(OpenedCandidate { file, fingerprint })
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "candidate is not a regular file",
        ))
    }
}

pub fn requires_path_identity(policy: PathnameReopenPolicy) -> bool {
    #[cfg(any(test, feature = "bench-internals"))]
    {
        policy != PathnameReopenPolicy::Off
    }
    #[cfg(not(any(test, feature = "bench-internals")))]
    {
        let _ = policy;
        false
    }
}
