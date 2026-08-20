use std::sync::Arc;

use crate::path::ResolvedPath;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: Arc<ResolvedPath>,
}

#[cfg(any(test, feature = "bench-internals"))]
use super::request::{CandidatePolicy, GrepError, GrepMemoryPolicy};

#[cfg(any(test, feature = "bench-internals"))]
use crate::runtime::MemoryReservation;

#[cfg(any(test, feature = "bench-internals"))]
#[allow(dead_code)]
pub struct CandidateSet {
    candidates: Vec<Candidate>,
    _memory: Option<MemoryReservation>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CandidateMetrics {
    pub count: usize,
    pub estimated_retained_bytes: usize,
    pub vec_capacity: usize,
    pub soft_target_crossings: usize,
    pub key_bytes: usize,
    pub key_capacity: usize,
    pub capability_key_bytes: usize,
    pub capability_key_capacity: usize,
    pub absolute_bytes: usize,
    pub absolute_capacity: usize,
    pub sort_key_bytes: usize,
    pub sort_key_capacity: usize,
    pub slash_path_bytes: usize,
    pub slash_path_capacity: usize,
}

#[cfg(any(test, feature = "bench-internals"))]
#[allow(dead_code)]
pub struct CandidateCollection {
    candidates: Vec<Candidate>,
    policy: CandidatePolicy,
    memory_limit: usize,
    reservation_base_bytes: usize,
    memory: Option<MemoryReservation>,
    pub estimated_retained_bytes: usize,
    pub soft_target_crossings: usize,
    key_bytes: usize,
    key_capacity: usize,
    capability_key_bytes: usize,
    capability_key_capacity: usize,
    absolute_bytes: usize,
    absolute_capacity: usize,
    sort_key_bytes: usize,
    sort_key_capacity: usize,
    slash_path_bytes: usize,
    slash_path_capacity: usize,
}

#[cfg(any(test, feature = "bench-internals"))]
#[allow(dead_code)]
impl CandidateCollection {
    pub fn new(
        policy: CandidatePolicy,
        memory_policy: GrepMemoryPolicy,
        memory: Option<MemoryReservation>,
    ) -> Self {
        let candidates = Vec::with_capacity(1_024);
        let estimated_retained_bytes = candidates
            .capacity()
            .saturating_mul(std::mem::size_of::<Candidate>());
        Self {
            candidates,
            policy,
            memory_limit: memory_policy.candidate_limit_bytes(),
            reservation_base_bytes: memory_policy.base_reservation_bytes(),
            memory,
            estimated_retained_bytes,
            soft_target_crossings: 0,
            key_bytes: 0,
            key_capacity: 0,
            capability_key_bytes: 0,
            capability_key_capacity: 0,
            absolute_bytes: 0,
            absolute_capacity: 0,
            sort_key_bytes: 0,
            sort_key_capacity: 0,
            slash_path_bytes: 0,
            slash_path_capacity: 0,
        }
    }

    pub fn admit(&mut self, candidate: Candidate) -> Result<(), GrepError> {
        let components = candidate.path.memory_components();
        self.candidates
            .try_reserve(1)
            .map_err(|_| GrepError::CandidateMemory)?;
        let path_retained = self
            .estimated_retained_bytes
            .saturating_add(components.key_capacity)
            .saturating_add(components.capability_key_capacity)
            .saturating_add(components.absolute_capacity)
            .saturating_add(components.sort_key_capacity)
            .saturating_add(components.slash_path_capacity)
            .saturating_add(std::mem::size_of::<ResolvedPath>());
        let retained = path_retained.saturating_add(
            self.candidates
                .capacity()
                .saturating_mul(std::mem::size_of::<Candidate>()),
        );
        let hard_limit = self.memory_limit.min(match self.policy {
            #[cfg(feature = "bench-internals")]
            CandidatePolicy::FatalCeiling => super::request::CANDIDATE_SOFT_TARGET_BYTES,
            CandidatePolicy::SoftTarget => self.memory_limit,
        });
        if retained > hard_limit {
            return Err(GrepError::CandidateMemory);
        }
        if self.memory.as_mut().is_some_and(|memory| {
            !memory.try_grow_to(self.reservation_base_bytes.saturating_add(retained))
        }) {
            return Err(GrepError::MemoryBusy);
        }
        self.estimated_retained_bytes = retained;
        self.key_bytes = self.key_bytes.saturating_add(components.key_bytes);
        self.key_capacity = self.key_capacity.saturating_add(components.key_capacity);
        self.capability_key_bytes = self
            .capability_key_bytes
            .saturating_add(components.capability_key_bytes);
        self.capability_key_capacity = self
            .capability_key_capacity
            .saturating_add(components.capability_key_capacity);
        self.absolute_bytes = self
            .absolute_bytes
            .saturating_add(components.absolute_bytes);
        self.absolute_capacity = self
            .absolute_capacity
            .saturating_add(components.absolute_capacity);
        self.sort_key_bytes = self
            .sort_key_bytes
            .saturating_add(components.sort_key_bytes);
        self.sort_key_capacity = self
            .sort_key_capacity
            .saturating_add(components.sort_key_capacity);
        self.slash_path_bytes = self
            .slash_path_bytes
            .saturating_add(components.slash_path_bytes);
        self.slash_path_capacity = self
            .slash_path_capacity
            .saturating_add(components.slash_path_capacity);
        self.candidates.push(candidate);
        #[cfg(feature = "bench-internals")]
        if retained > super::request::CANDIDATE_SOFT_TARGET_BYTES {
            self.soft_target_crossings = self.soft_target_crossings.saturating_add(1);
        }
        Ok(())
    }

    pub fn metrics(&self) -> CandidateMetrics {
        CandidateMetrics {
            count: self.candidates.len(),
            estimated_retained_bytes: self.estimated_retained_bytes,
            vec_capacity: self.candidates.capacity(),
            soft_target_crossings: self.soft_target_crossings,
            key_bytes: self.key_bytes,
            key_capacity: self.key_capacity,
            capability_key_bytes: self.capability_key_bytes,
            capability_key_capacity: self.capability_key_capacity,
            absolute_bytes: self.absolute_bytes,
            absolute_capacity: self.absolute_capacity,
            sort_key_bytes: self.sort_key_bytes,
            sort_key_capacity: self.sort_key_capacity,
            slash_path_bytes: self.slash_path_bytes,
            slash_path_capacity: self.slash_path_capacity,
        }
    }

    pub fn into_set(
        self,
        _traversal: crate::traversal::TraversalSummary,
        _single_file: bool,
    ) -> CandidateSet {
        CandidateSet {
            candidates: self.candidates,
            _memory: self.memory,
        }
    }
}

pub fn candidate(path: ResolvedPath) -> Result<Candidate, super::request::GrepError> {
    path.absolute().to_str().ok_or_else(|| {
        super::request::GrepError::Validation("candidate path is not valid Unicode".to_owned())
    })?;
    Ok(Candidate {
        path: Arc::new(path),
    })
}
