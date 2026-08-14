use std::{cmp::Ordering, collections::BinaryHeap};

use tokio_util::sync::CancellationToken;

use crate::{
    output::{OutputFormatter, OutputLimits, search_tail},
    path::{PathSortKey, ResolvedPath},
    sorting,
    tools::ToolOutput,
    traversal::{TraversalError, TraversalSummary},
};

use super::request::{DEFAULT_LIMIT, GlobError, GlobRequest, MAX_MATCHES, PATH_OMISSION};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct GlobMatch {
    pub(super) sort_key: PathSortKey,
    pub(super) absolute: String,
    pub(super) charge: usize,
}

impl Ord for GlobMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key
            .cmp(&other.sort_key)
            .then_with(|| self.absolute.cmp(&other.absolute))
    }
}

impl PartialOrd for GlobMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(super) struct TopK {
    capacity: usize,
    heap: BinaryHeap<GlobMatch>,
    charged: usize,
    memory_limit: usize,
    reservation: Option<crate::runtime::MemoryReservation>,
}

impl TopK {
    pub(super) fn new(
        capacity: usize,
        memory_limit: usize,
        mut reservation: Option<crate::runtime::MemoryReservation>,
    ) -> Result<Self, GlobError> {
        let heap = BinaryHeap::with_capacity(capacity);
        let charged = heap
            .capacity()
            .saturating_mul(std::mem::size_of::<GlobMatch>());
        if charged > memory_limit {
            return Err(GlobError::Memory);
        }
        if reservation
            .as_mut()
            .is_some_and(|reservation| !reservation.try_grow_to(charged))
        {
            return Err(GlobError::MemoryBusy);
        }
        Ok(Self {
            capacity,
            heap,
            charged,
            memory_limit,
            reservation,
        })
    }

    pub(super) fn threshold(&self) -> TopKThreshold {
        TopKThreshold {
            has_capacity: self.capacity > 0 && self.heap.len() < self.capacity,
            worst: self.heap.peek().map(|entry| entry.sort_key.clone()),
        }
    }

    pub(super) fn might_admit(&self, sort_key: &PathSortKey) -> bool {
        self.threshold().might_admit(sort_key)
    }

    pub(super) fn admit(&mut self, path: &ResolvedPath) -> Result<(), GlobError> {
        if self.capacity == 0 {
            return Ok(());
        }
        let absolute = crate::path::display_path(path.absolute());
        let charge = absolute
            .capacity()
            .saturating_add(path.sort_key().capacity_bytes());
        let candidate = GlobMatch {
            sort_key: path.sort_key().clone(),
            absolute,
            charge,
        };
        if self.heap.len() < self.capacity {
            self.charge(charge)?;
            self.heap.push(candidate);
            return Ok(());
        }
        let Some(worst) = self.heap.peek() else {
            return Ok(());
        };
        if candidate >= *worst {
            return Ok(());
        }
        let new_charge = self
            .charged
            .saturating_sub(worst.charge)
            .saturating_add(charge);
        if new_charge > self.memory_limit {
            return Err(GlobError::Memory);
        }
        self.reserve(new_charge)?;
        self.heap.pop();
        self.heap.push(candidate);
        self.charged = new_charge;
        Ok(())
    }

    pub(super) fn len(&self) -> usize {
        self.heap.len()
    }

    pub(super) fn retained_memory_bytes(&self) -> usize {
        self.charged
    }

    fn charge(&mut self, charge: usize) -> Result<(), GlobError> {
        let total = self.charged.saturating_add(charge);
        if total > self.memory_limit {
            return Err(GlobError::Memory);
        }
        self.reserve(total)?;
        self.charged = total;
        Ok(())
    }

    fn reserve(&mut self, total: usize) -> Result<(), GlobError> {
        if self
            .reservation
            .as_mut()
            .is_some_and(|reservation| !reservation.try_grow_to(total))
        {
            return Err(GlobError::MemoryBusy);
        }
        Ok(())
    }

    pub(super) fn into_sorted(
        self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<GlobMatch>, GlobError> {
        let mut retained = self.heap.into_vec();
        sorting::sort_by(&mut retained, cancellation, Ord::cmp)
            .map_err(|_| TraversalError::Cancelled)?;
        Ok(retained)
    }
}

#[derive(Clone)]
pub(super) struct TopKThreshold {
    has_capacity: bool,
    worst: Option<PathSortKey>,
}

impl TopKThreshold {
    pub(super) fn might_admit(&self, sort_key: &PathSortKey) -> bool {
        self.has_capacity || self.worst.as_ref().is_some_and(|worst| sort_key <= worst)
    }
}

fn page_tail(
    summary: &TraversalSummary,
    scan_stopped: bool,
    next_offset: Option<usize>,
) -> Vec<String> {
    let extras = scan_stopped.then(|| {
        format!("Scan stopped: more than {MAX_MATCHES} paths matched; narrow pattern or path.")
    });
    search_tail(
        &summary.skips,
        !scan_stopped && next_offset.is_none(),
        "entries",
        extras,
        next_offset,
    )
}

#[cfg(test)]
pub(super) fn render(
    request: &GlobRequest,
    retained: &[GlobMatch],
    total: usize,
    summary: &TraversalSummary,
    scan_stopped: bool,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, GlobError> {
    render_with_budget(
        request,
        retained,
        total,
        summary,
        scan_stopped,
        cancellation,
        None,
    )
}

pub(super) fn render_with_budget(
    request: &GlobRequest,
    retained: &[GlobMatch],
    total: usize,
    summary: &TraversalSummary,
    scan_stopped: bool,
    cancellation: &CancellationToken,
    output_budget: Option<&crate::output::CallOutputBudget>,
) -> Result<ToolOutput, GlobError> {
    let standalone;
    let output_budget = if let Some(output_budget) = output_budget {
        output_budget
    } else {
        standalone = crate::output::CallOutputBudget::standalone();
        &standalone
    };
    let offset = request.offset.unwrap_or(0);
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let available = retained.len().saturating_sub(offset).min(limit);
    let limits = OutputLimits::for_content_parts(
        retained
            .iter()
            .skip(offset)
            .take(available)
            .map(|matched| matched.absolute.as_str()),
    );
    let mut cap = available;
    loop {
        let next_offset = (offset.saturating_add(cap) < total).then(|| offset.saturating_add(cap));
        let tail = page_tail(summary, scan_stopped, next_offset);
        let header = if available == 0 {
            if offset == 0 {
                "No paths matched.".to_owned()
            } else {
                format!("No results at offset={offset}.")
            }
        } else {
            String::new()
        };
        let mut formatter = OutputFormatter::new(header, tail, limits)?;
        let mut shown = 0_usize;
        for matched in retained.iter().skip(offset).take(cap) {
            if formatter.try_push_line(&matched.absolute, cancellation)? {
                shown += 1;
                continue;
            }
            if shown == 0 && formatter.try_push_line(PATH_OMISSION, cancellation)? {
                shown += 1;
            }
            break;
        }
        if shown < cap {
            cap = shown;
            continue;
        }
        let output = ToolOutput::new(formatter.finish(cancellation)?);
        if output.fits_budget_and_call(output_budget, cancellation) {
            return Ok(output);
        }
        if cap == 1 {
            let next_offset = (offset.saturating_add(1) < total).then(|| offset.saturating_add(1));
            let tail = page_tail(summary, scan_stopped, next_offset);
            let mut formatter = OutputFormatter::new(String::new(), tail, limits)?;
            if formatter.try_push_line(PATH_OMISSION, cancellation)? {
                let fallback = ToolOutput::new(formatter.finish(cancellation)?);
                if fallback.fits_budget_and_call(output_budget, cancellation) {
                    return Ok(fallback);
                }
            }
        }
        if cap == 0 {
            return Err(crate::output::OutputError::BurstLimit.into());
        }
        cap -= 1;
    }
}
