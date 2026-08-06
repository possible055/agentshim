#[derive(Clone, Debug, Eq, PartialEq)]
struct GlobMatch {
    sort_key: PathSortKey,
    absolute: String,
    charge: usize,
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

struct TopK {
    capacity: usize,
    heap: BinaryHeap<GlobMatch>,
    charged: usize,
}

impl TopK {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            heap: BinaryHeap::with_capacity(capacity),
            charged: 0,
        }
    }

    fn admit(&mut self, path: &ResolvedPath) -> Result<(), GlobError> {
        if self.capacity == 0 {
            return Ok(());
        }
        let absolute = path
            .absolute()
            .to_str()
            .ok_or_else(|| GlobError::Validation("matched path is not valid Unicode".to_owned()))?
            .to_owned();
        let charge = absolute
            .len()
            .saturating_add(path.key().as_os_str().len())
            .saturating_add(std::mem::size_of::<GlobMatch>());
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
        if new_charge > RETAINED_MEMORY_BYTES {
            return Err(GlobError::Memory);
        }
        self.heap.pop();
        self.heap.push(candidate);
        self.charged = new_charge;
        Ok(())
    }

    fn charge(&mut self, charge: usize) -> Result<(), GlobError> {
        let total = self.charged.saturating_add(charge);
        if total > RETAINED_MEMORY_BYTES {
            return Err(GlobError::Memory);
        }
        self.charged = total;
        Ok(())
    }

    fn into_sorted(self, cancellation: &CancellationToken) -> Result<Vec<GlobMatch>, GlobError> {
        let mut retained = self.heap.into_vec();
        sorting::sort_by(&mut retained, cancellation, Ord::cmp)
            .map_err(|_| TraversalError::Cancelled)?;
        Ok(retained)
    }
}

fn render(
    request: &GlobRequest,
    retained: &[GlobMatch],
    total: usize,
    summary: TraversalSummary,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, GlobError> {
    let offset = request.offset.unwrap_or(0);
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let available = retained.len().saturating_sub(offset).min(limit);
    let mut cap = available;
    loop {
        let next_offset = (offset.saturating_add(cap) < total)
            .then(|| offset.saturating_add(cap));
        let mut tail = Vec::new();
        if let Some(line) = summary.model_line() {
            tail.push(line);
        }
        tail.push(next_offset.map_or_else(
            || "Complete.".to_owned(),
            |next| format!("Partial: next_offset={next}."),
        ));
        let mut formatter = OutputFormatter::new(
            format!("Pattern: {}", request.pattern),
            tail,
            OutputLimits::default(),
        )?;
        let mut items = Vec::with_capacity(cap);
        for matched in retained.iter().skip(offset).take(cap) {
            if formatter.try_push_line(&matched.absolute, cancellation)? {
                items.push(GlobItem {
                    path: matched.absolute.clone(),
                });
                continue;
            }
            if items.is_empty() && formatter.try_push_line(PATH_OMISSION, cancellation)? {
                items.push(GlobItem {
                    path: PATH_OMISSION.to_owned(),
                });
            }
            break;
        }
        if items.len() < cap {
            cap = items.len();
            continue;
        }
        let result = GlobResult {
            items,
            total,
            offset,
            limit,
            next_offset,
            skipped: summary,
        };
        let output = ToolOutput::new(formatter.finish(cancellation)?, &result)?;
        if output.fits_budget() {
            return Ok(output);
        }
        if cap == 0 {
            return Err(crate::output::OutputError::NoProgress.into());
        }
        cap -= 1;
    }
}

include!("tests.rs");
