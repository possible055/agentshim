use std::collections::BinaryHeap;

use tokio_util::sync::CancellationToken;

use crate::{
    output::{OutputFormatter, OutputLimits, search_tail},
    path::ResolvedPath,
    runtime::MemoryReservation,
    tools::ToolOutput,
    traversal::TraversalSummary,
};

use super::request::{DEFAULT_LIMIT, GlobError, GlobRequest, MAX_MATCHES, PATH_OMISSION};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GlobMatch {
    pub absolute: String,
    pub charge: usize,
}

pub struct BoundedCollector {
    capacity: usize,
    matches: BinaryHeap<GlobMatch>,
    charged: usize,
    memory_limit: usize,
    reservation: Option<MemoryReservation>,
}

impl BoundedCollector {
    pub fn new(
        capacity: usize,
        memory_limit: usize,
        mut reservation: Option<MemoryReservation>,
    ) -> Result<Self, GlobError> {
        let matches = BinaryHeap::with_capacity(capacity);
        let charged = matches
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
            matches,
            charged,
            memory_limit,
            reservation,
        })
    }

    pub fn admit(&mut self, path: &ResolvedPath) -> Result<(), GlobError> {
        let absolute = crate::path::display_path(path.absolute());
        let charge = absolute.capacity();
        let replaced_charge = if self.matches.len() >= self.capacity {
            let Some(largest) = self.matches.peek() else {
                return Ok(());
            };
            if absolute >= largest.absolute {
                return Ok(());
            }
            largest.charge
        } else {
            0
        };
        let total = self
            .charged
            .saturating_sub(replaced_charge)
            .saturating_add(charge);
        if total > self.memory_limit {
            return Err(GlobError::Memory);
        }
        if self
            .reservation
            .as_mut()
            .is_some_and(|reservation| !reservation.try_grow_to(total))
        {
            return Err(GlobError::MemoryBusy);
        }
        if replaced_charge > 0 {
            self.matches.pop();
        }
        self.matches.push(GlobMatch { absolute, charge });
        self.charged = total;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.matches.len()
    }

    pub fn retained_memory_bytes(&self) -> usize {
        self.charged
    }

    pub fn into_order(self) -> Vec<GlobMatch> {
        self.matches.into_sorted_vec()
    }
}

fn page_tail(
    summary: &TraversalSummary,
    scan_stopped: bool,
    total: usize,
    next_offset: Option<usize>,
    nothing_matched: bool,
) -> Vec<String> {
    let mut extras = Vec::new();
    if scan_stopped {
        if total >= MAX_MATCHES {
            extras.push(format!(
                "Scan stopped: more than {MAX_MATCHES} paths matched; narrow pattern or path."
            ));
        } else {
            extras.push("Scan stopped: page limit reached; narrow pattern or path.".to_owned());
        }
    }
    if nothing_matched && summary.gitignore_filtered {
        extras.push(crate::output::GITIGNORE_RETRY_HINT.to_owned());
    }
    search_tail(
        &summary.skips,
        !scan_stopped && next_offset.is_none(),
        "entries",
        extras,
        next_offset,
    )
}

#[cfg(test)]
pub fn render(
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
        &crate::output::TestCallBudget::default(),
    )
}

pub fn render_with_budget(
    request: &GlobRequest,
    retained: &[GlobMatch],
    total: usize,
    summary: &TraversalSummary,
    scan_stopped: bool,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<ToolOutput, GlobError> {
    let offset = request.offset.unwrap_or(0);
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let available = retained.len().saturating_sub(offset).min(limit);
    let limits = OutputLimits::for_content_parts_within(
        retained
            .iter()
            .skip(offset)
            .take(available)
            .map(|matched| matched.absolute.as_str()),
        output_budget.page_bytes(),
    );
    let mut cap = available;
    loop {
        let next_offset = (offset.saturating_add(cap) < total).then(|| offset.saturating_add(cap));
        let tail = page_tail(summary, scan_stopped, total, next_offset, total == 0);
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
            let tail = page_tail(summary, scan_stopped, total, next_offset, total == 0);
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
