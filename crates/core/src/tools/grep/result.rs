pub struct PageLine {
    pub text: String,
    fallback: Option<String>,
    sort_key: Option<ResultLineKey>,
}

impl PageLine {
    fn charge(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.text.len())
            .saturating_add(self.fallback.as_deref().map_or(0, str::len))
            .saturating_add(self.sort_key.as_ref().map_or(0, ResultLineKey::charge))
    }
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum ResultLineKey {
    Path(String),
    Content {
        path: String,
        line: u64,
        kind: RecordKind,
    },
}

impl ResultLineKey {
    fn charge(&self) -> usize {
        match self {
            Self::Path(path) | Self::Content { path, .. } => path.len(),
        }
    }
}

pub struct Page {
    pub lines: Vec<PageLine>,
    pub seen_entries: usize,
    skips: SkipNotes,
    offset: usize,
    retained_base_offset: usize,
    retain: usize,
    pub charged: usize,
    pub retaining: bool,
    pub scan_complete: bool,
    probe: usize,
    allow_early_retirement: bool,
    order: ReductionOrder,
    traversal: TraversalSummary,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ReductionOrder {
    Arrival,
    DeterministicTopK,
}

impl Page {
    pub fn new(
        request: &GrepRequest,
        traversal: TraversalSummary,
        allow_early_retirement: bool,
        deterministic_top_k: bool,
    ) -> Self {
        let offset = request.offset.unwrap_or(0);
        let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
        Self {
            lines: Vec::new(),
            seen_entries: 0,
            skips: SkipNotes::default(),
            offset,
            retained_base_offset: 0,
            retain: if deterministic_top_k {
                offset.saturating_add(limit).saturating_add(1)
            } else {
                limit.saturating_add(1)
            },
            charged: 0,
            retaining: true,
            scan_complete: false,
            probe: offset.saturating_add(limit).saturating_add(1),
            allow_early_retirement,
            order: if deterministic_top_k {
                ReductionOrder::DeterministicTopK
            } else {
                ReductionOrder::Arrival
            },
            traversal,
        }
    }

    pub fn mark_complete(&mut self) {
        self.scan_complete = true;
    }

    pub fn set_traversal_summary(&mut self, traversal: TraversalSummary) {
        self.traversal = traversal;
    }

    pub fn reduce(
        &mut self,
        outcome: FileOutcome,
        mode: GrepMode,
        single_file: bool,
    ) -> Result<ReduceControl, GrepError> {
        let FileOutcome {
            path,
            records,
            entries,
            occurrences,
            matched,
            skip,
            retired,
            leading_skipped,
            retry,
        } = outcome;
        debug_assert!(!retired, "retired outcomes must not reach the reducer");
        debug_assert!(retry.is_none(), "retry outcomes must not reach the reducer");
        if self.order == ReductionOrder::DeterministicTopK
            && self.seen_entries == 0
            && self.lines.is_empty()
        {
            self.retained_base_offset = leading_skipped.min(self.offset);
        }
        self.seen_entries = self.seen_entries.saturating_add(leading_skipped);
        if let Some(reason) = skip {
            if single_file {
                return Err(GrepError::Unsearchable(reason));
            }
            self.skips
                .record(display_skip_path(path.as_deref()), reason);
            if records.is_empty() {
                return Ok(ReduceControl::Continue);
            }
        }
        let deterministic = self.order == ReductionOrder::DeterministicTopK;
        match mode {
            GrepMode::Files if matched => {
                self.push_entry_lazy(|| {
                    let path = display_outcome_path(path.as_deref());
                    let key = deterministic.then(|| ResultLineKey::Path(path.clone()));
                    (path, None, key)
                });
            }
            GrepMode::Count if matched => {
                self.push_entry_lazy(|| {
                    let path = display_outcome_path(path.as_deref());
                    let key = deterministic.then(|| ResultLineKey::Path(path.clone()));
                    (format!("{path}:{occurrences}"), None, key)
                });
            }
            GrepMode::Content => {
                let captured = records.len();
                let mut absolute = None;
                for record in records {
                    let separator = if record.kind == RecordKind::Match {
                        ':'
                    } else {
                        '-'
                    };
                    self.push_entry_lazy(|| {
                        let absolute =
                            absolute.get_or_insert_with(|| display_outcome_path(path.as_deref()));
                        let fallback = format!(
                            "{absolute}{separator}{}{separator}{CONTENT_OMISSION}",
                            record.line
                        );
                        let key = deterministic.then(|| ResultLineKey::Content {
                            path: absolute.clone(),
                            line: record.line,
                            kind: record.kind,
                        });
                        (
                            format!(
                                "{absolute}{separator}{}{separator}{}",
                                record.line, record.text
                            ),
                            Some(fallback),
                            key,
                        )
                    });
                    if self.page_full() {
                        return Ok(ReduceControl::PageFull);
                    }
                }
                self.seen_entries = self.seen_entries.saturating_add(
                    entries
                        .saturating_sub(captured)
                        .saturating_sub(leading_skipped),
                );
            }
            GrepMode::Files | GrepMode::Count => {}
        }
        Ok(if self.page_full() {
            ReduceControl::PageFull
        } else {
            ReduceControl::Continue
        })
    }

    #[cfg(test)]
    pub fn push_entry(&mut self, line: String, detailed_fallback: Option<String>) {
        self.push_entry_lazy(|| (line, detailed_fallback, None));
    }

    fn push_entry_lazy(
        &mut self,
        build: impl FnOnce() -> (String, Option<String>, Option<ResultLineKey>),
    ) {
        let should_consider = if self.order == ReductionOrder::DeterministicTopK {
            self.retain > 0
        } else {
            self.seen_entries >= self.offset && self.lines.len() < self.retain && self.retaining
        };
        if should_consider {
            let (line, detailed_fallback, sort_key) = build();
            let fallback = detailed_fallback.unwrap_or_else(|| GENERIC_OMISSION.to_owned());
            if self.can_retain_best(&line, Some(&fallback), sort_key.as_ref()) {
                self.retain_best(PageLine {
                    text: line,
                    fallback: Some(fallback),
                    sort_key,
                });
            } else if fallback != GENERIC_OMISSION
                && self.can_retain_best(&fallback, Some(GENERIC_OMISSION), sort_key.as_ref())
            {
                self.retain_best(PageLine {
                    text: fallback,
                    fallback: Some(GENERIC_OMISSION.to_owned()),
                    sort_key,
                });
            } else if self.can_retain_best(GENERIC_OMISSION, None, sort_key.as_ref()) {
                self.retain_best(PageLine {
                    text: GENERIC_OMISSION.to_owned(),
                    fallback: None,
                    sort_key,
                });
            } else {
                self.retaining = false;
            }
        }
        self.seen_entries = self.seen_entries.saturating_add(1);
    }

    fn page_full(&self) -> bool {
        self.allow_early_retirement && self.seen_entries >= self.probe
    }

    pub fn exact_search_window(&self) -> (usize, usize) {
        (
            self.offset.saturating_sub(self.seen_entries),
            self.probe.saturating_sub(self.seen_entries),
        )
    }

    fn can_retain_best(
        &self,
        text: &str,
        fallback: Option<&str>,
        sort_key: Option<&ResultLineKey>,
    ) -> bool {
        if self.order == ReductionOrder::DeterministicTopK
            && self.lines.len() >= self.retain
            && self.lines.last().is_some_and(|largest| {
                compare_result_lines(sort_key, text, largest.sort_key.as_ref(), &largest.text)
                    != Ordering::Less
            })
        {
            return false;
        }
        let charge = std::mem::size_of::<PageLine>()
            .saturating_add(text.len())
            .saturating_add(fallback.map_or(0, str::len))
            .saturating_add(sort_key.map_or(0, ResultLineKey::charge));
        let replaced =
            if self.order == ReductionOrder::DeterministicTopK && self.lines.len() >= self.retain {
                self.lines.last().map_or(0, PageLine::charge)
            } else {
                0
            };
        self.charged.saturating_sub(replaced).saturating_add(charge) <= PAGE_MEMORY_BYTES
    }

    fn retain_best(&mut self, line: PageLine) {
        if self.order == ReductionOrder::DeterministicTopK
            && self.lines.len() >= self.retain
            && let Some(removed) = self.lines.pop()
        {
            self.charged = self.charged.saturating_sub(removed.charge());
        }
        self.charged = self.charged.saturating_add(line.charge());
        if self.order == ReductionOrder::DeterministicTopK {
            let index = self
                .lines
                .binary_search_by(|existing| compare_page_lines(existing, &line))
                .unwrap_or_else(|index| index);
            self.lines.insert(index, line);
        } else {
            self.lines.push(line);
        }
    }

    fn combined_skips(&self) -> SkipNotes {
        let mut notes = self.skips.clone();
        notes.merge(&self.traversal.skips);
        notes
    }
}

fn compare_page_lines(left: &PageLine, right: &PageLine) -> Ordering {
    compare_result_lines(
        left.sort_key.as_ref(),
        &left.text,
        right.sort_key.as_ref(),
        &right.text,
    )
}

fn compare_result_lines(
    left_key: Option<&ResultLineKey>,
    left_text: &str,
    right_key: Option<&ResultLineKey>,
    right_text: &str,
) -> Ordering {
    match (left_key, right_key) {
        (Some(left), Some(right)) => left.cmp(right).then_with(|| left_text.cmp(right_text)),
        _ => left_text.cmp(right_text),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReduceControl {
    Continue,
    PageFull,
}

fn display_outcome_path(path: Option<&ResolvedPath>) -> String {
    crate::path::display_path(
        path.expect("matched grep outcome retains its admitted path")
            .absolute(),
    )
}

fn display_skip_path(path: Option<&ResolvedPath>) -> String {
    path.map_or_else(
        || "<unknown>".to_owned(),
        |path| crate::path::display_path(path.absolute()),
    )
}

fn page_tail(
    notes: &SkipNotes,
    next_offset: Option<usize>,
    scan_complete: bool,
    nothing_matched: bool,
    offer_fallback_encoding: bool,
) -> Vec<String> {
    let mut extras = Vec::new();
    if nothing_matched {
        extras.push(crate::output::GITIGNORE_RETRY_HINT.to_owned());
    }
    if offer_fallback_encoding && notes.has_undecodable() {
        extras.push(crate::output::UNDECODABLE_RETRY_HINT.to_owned());
    }
    crate::output::search_tail(notes, scan_complete, "files", extras, next_offset)
}

#[cfg(test)]
pub fn render(
    request: &GrepRequest,
    page: &Page,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, GrepError> {
    render_with_budget(
        request,
        page,
        cancellation,
        &crate::output::TestCallBudget::default(),
    )
}

pub fn render_with_budget(
    request: &GrepRequest,
    page: &Page,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<ToolOutput, GrepError> {
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let retained_offset = if page.order == ReductionOrder::DeterministicTopK {
        page.offset.saturating_sub(page.retained_base_offset)
    } else {
        0
    };
    let available = page.lines.len().saturating_sub(retained_offset).min(limit);
    let limits = OutputLimits::for_content_parts_within(
        page.lines
            .iter()
            .skip(retained_offset)
            .take(available)
            .map(|line| line.text.as_str()),
        output_budget.page_bytes(),
    );
    let mut cap = available;
    loop {
        let notes = page.combined_skips();
        let shown_end = page.offset.saturating_add(cap);
        let next_offset =
            (!page.scan_complete || shown_end < page.seen_entries).then_some(shown_end);
        // Only a finished scan can claim nothing matched. A truncated one already tells the
        // caller to continue, and pointing at gitignore there would send them the wrong way.
        let nothing_matched =
            page.scan_complete && page.seen_entries == 0 && page.traversal.gitignore_filtered;
        // Offering the argument the caller already supplied would just be noise.
        let offer_fallback_encoding = request.fallback_encoding.is_none();
        let tail = page_tail(
            &notes,
            next_offset,
            page.scan_complete,
            nothing_matched,
            offer_fallback_encoding,
        );
        let header = if available == 0 {
            if page.offset == 0 {
                "No matches.".to_owned()
            } else {
                format!("No results at offset={}.", page.offset)
            }
        } else {
            String::new()
        };
        let mut formatter = OutputFormatter::new(header, tail, limits)?;
        let mut shown = 0_usize;
        for line in page.lines.iter().skip(retained_offset).take(cap) {
            if formatter.try_push_line(&line.text, cancellation)? {
                shown += 1;
                continue;
            }
            if shown == 0
                && let Some(fallback) = line.fallback.as_deref()
                && formatter.try_push_line(fallback, cancellation)?
            {
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
        if cap == 1
            && let Some(fallback) = page.lines[retained_offset].fallback.as_deref()
        {
            let tail = page_tail(
                &notes,
                next_offset,
                page.scan_complete,
                nothing_matched,
                offer_fallback_encoding,
            );
            let mut formatter = OutputFormatter::new(String::new(), tail, limits)?;
            if !formatter.try_push_line(fallback, cancellation)? {
                return Err(crate::output::OutputError::BurstLimit.into());
            }
            let fallback_output = ToolOutput::new(formatter.finish(cancellation)?);
            if fallback_output.fits_budget_and_call(output_budget, cancellation) {
                return Ok(fallback_output);
            }
        }
        if cap == 0 {
            return Err(crate::output::OutputError::BurstLimit.into());
        }
        cap -= 1;
    }
}

use tokio_util::sync::CancellationToken;

use crate::{
    output::{OutputFormatter, OutputLimits, SkipNotes},
    path::ResolvedPath,
    tools::ToolOutput,
    traversal::TraversalSummary,
};

use super::{
    file_search::{FileOutcome, RecordKind},
    request::{
        CONTENT_OMISSION, DEFAULT_LIMIT, GENERIC_OMISSION, GrepError, GrepMode, GrepRequest,
        PAGE_MEMORY_BYTES,
    },
};
use std::cmp::Ordering;

#[cfg(test)]
mod result_order_tests {
    use super::*;

    #[test]
    fn structured_order_does_not_parse_numeric_filename_segments() {
        let first = PageLine {
            text: "file-10-a.rs:10:needle".to_owned(),
            fallback: None,
            sort_key: Some(ResultLineKey::Content {
                path: "file-10-a.rs".to_owned(),
                line: 10,
                kind: RecordKind::Match,
            }),
        };
        let second = PageLine {
            text: "file-2-z.rs:2:needle".to_owned(),
            fallback: None,
            sort_key: Some(ResultLineKey::Content {
                path: "file-2-z.rs".to_owned(),
                line: 2,
                kind: RecordKind::Match,
            }),
        };

        assert_eq!(compare_page_lines(&first, &second), Ordering::Less);
    }
}
