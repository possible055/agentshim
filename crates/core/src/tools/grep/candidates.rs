use std::sync::Arc;

use crate::path::ResolvedPath;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: Arc<ResolvedPath>,
}

#[cfg(test)]
use super::request::{GrepError, GrepMemoryPolicy};

#[cfg(test)]
use crate::runtime::MemoryReservation;

#[cfg(test)]
pub struct CandidateCollection {
    candidates: Vec<Candidate>,
    memory_limit: usize,
    reservation_base_bytes: usize,
    memory: Option<MemoryReservation>,
    pub estimated_retained_bytes: usize,
}

#[cfg(test)]
impl CandidateCollection {
    pub fn new(memory_policy: GrepMemoryPolicy, memory: Option<MemoryReservation>) -> Self {
        let candidates = Vec::with_capacity(1_024);
        let estimated_retained_bytes = candidates
            .capacity()
            .saturating_mul(std::mem::size_of::<Candidate>());
        Self {
            candidates,
            memory_limit: memory_policy.candidate_limit_bytes(),
            reservation_base_bytes: memory_policy.base_reservation_bytes(),
            memory,
            estimated_retained_bytes,
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
        if retained > self.memory_limit {
            return Err(GrepError::CandidateMemory);
        }
        if self.memory.as_mut().is_some_and(|memory| {
            !memory.try_grow_to(self.reservation_base_bytes.saturating_add(retained))
        }) {
            return Err(GrepError::MemoryBusy);
        }
        self.estimated_retained_bytes = retained;
        self.candidates.push(candidate);
        Ok(())
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
