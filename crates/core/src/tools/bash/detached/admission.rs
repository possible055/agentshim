//! The detached admission protocol: slot reservation, log-path exclusivity,
//! commit-or-rollback, and the durable log file pair.

use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(any(test, feature = "test-hooks"))]
use std::io;

use super::{RosterState, stopping_admission_error};
use crate::{
    path::{RepositoryRoot, ResolvedPath},
    platform::process::DetachedTree,
    tools::exec::{ProcessError, spawn::CLEANUP_DEADLINE},
};

pub struct DetachedAdmission {
    trees: super::DetachedTrees,
    job_id: String,
    reserved_key: Option<String>,
    settled: bool,
}

impl DetachedAdmission {
    pub(super) fn new(trees: super::DetachedTrees, job_id: String) -> Self {
        Self {
            trees,
            job_id,
            reserved_key: None,
            settled: false,
        }
    }

    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    pub fn reserve_log_path(&mut self, path: &Path) -> Result<(), ProcessError> {
        let key = normalized_log_key(path);
        let mut state = self.trees.lock();
        if state.state != RosterState::Accepting {
            return Err(stopping_admission_error());
        }
        if state.reserved_paths.iter().any(|held| held == &key)
            || state.active.iter().any(|job| job.log_key == key)
        {
            return Err(ProcessError::ResourceBusy(format!(
                "log_path {} is already in use by an active or reserved detached call in this instance",
                path.display()
            )));
        }
        state.reserved_paths.push(key.clone());
        self.reserved_key = Some(key);
        Ok(())
    }

    pub fn retain(
        mut self,
        tree: DetachedTree,
        log_path: PathBuf,
        log_reader: Arc<File>,
        timeout: Duration,
        started_at: Instant,
    ) -> Result<(), DetachedTree> {
        match self.trees.commit(
            self.job_id.clone(),
            tree,
            log_path,
            log_reader,
            self.reserved_key.clone(),
            timeout,
            started_at,
        ) {
            Ok(()) => {
                self.settled = true;
                #[cfg(any(test, feature = "test-hooks"))]
                self.trees.run_after_commit_hook();
                Ok(())
            }
            Err(tree) => Err(tree),
        }
    }

    #[must_use]
    pub fn rollback_deadline(&self) -> Instant {
        let budget = self
            .trees
            .shutdown_deadline()
            .map_or(CLEANUP_DEADLINE, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(CLEANUP_DEADLINE)
            });
        Instant::now() + budget
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn before_open(&self) {
        self.trees.run_before_open_hook();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn after_open(&self) {
        self.trees.run_after_open_hook();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn injected_spawn_error(&self) -> Option<ProcessError> {
        self.trees.take_spawn_failure().then(|| {
            ProcessError::Io(io::Error::other(
                "injected detached spawn failure for lifecycle testing",
            ))
        })
    }
}

impl Drop for DetachedAdmission {
    fn drop(&mut self) {
        if !self.settled {
            let key = self.reserved_key.take();
            self.trees.release(&self.job_id, key.as_deref());
        }
    }
}

pub(super) fn normalized_log_key(path: &Path) -> String {
    let rendered = path.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        rendered.to_ascii_lowercase().replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        rendered
    }
}

pub struct DetachedLog {
    pub writer: File,
    pub reader: Arc<File>,
}

pub fn open_log(root: &RepositoryRoot, path: &ResolvedPath) -> Result<DetachedLog, ProcessError> {
    let writer = root.create_truncated(path).map_err(|error| {
        ProcessError::Validation(format!(
            "cannot open log_path {}: {error}",
            path.absolute().display()
        ))
    })?;
    let reader = Arc::new(writer.try_clone().map_err(ProcessError::Io)?);
    Ok(DetachedLog { writer, reader })
}
