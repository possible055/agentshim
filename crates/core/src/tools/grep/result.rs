pub struct PageLine {
    pub text: String,
    fallback: Option<String>,
}

impl PageLine {
    fn charge(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.text.len())
            .saturating_add(self.fallback.as_deref().map_or(0, str::len))
    }
}

pub struct Page {
    pub lines: Vec<PageLine>,
    pub seen_entries: usize,
    skips: SkipNotes,
    offset: usize,
    retain: usize,
    pub charged: usize,
    pub retaining: bool,
    pub scan_complete: bool,
    probe: usize,
    allow_early_retirement: bool,
    traversal: TraversalSummary,
}

impl Page {
    pub fn new(
        request: &GrepRequest,
        traversal: TraversalSummary,
        allow_early_retirement: bool,
    ) -> Self {
        let offset = request.offset.unwrap_or(0);
        let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
        Self {
            lines: Vec::new(),
            seen_entries: 0,
            skips: SkipNotes::default(),
            offset,
            retain: limit.saturating_add(1),
            charged: 0,
            retaining: true,
            scan_complete: false,
            probe: offset.saturating_add(limit).saturating_add(1),
            allow_early_retirement,
            traversal,
        }
    }

    pub fn mark_complete(&mut self) {
        self.scan_complete = true;
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
        match mode {
            GrepMode::Files if matched => {
                self.push_entry_lazy(|| (display_outcome_path(path.as_deref()), None));
            }
            GrepMode::Count if matched => {
                self.push_entry_lazy(|| {
                    (
                        format!("{}:{occurrences}", display_outcome_path(path.as_deref())),
                        None,
                    )
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
                        (
                            format!(
                                "{absolute}{separator}{}{separator}{}",
                                record.line, record.text
                            ),
                            Some(fallback),
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
        self.push_entry_lazy(|| (line, detailed_fallback));
    }

    fn push_entry_lazy(&mut self, build: impl FnOnce() -> (String, Option<String>)) {
        if self.seen_entries >= self.offset && self.lines.len() < self.retain && self.retaining {
            let (line, detailed_fallback) = build();
            let fallback = detailed_fallback.unwrap_or_else(|| GENERIC_OMISSION.to_owned());
            if self.can_retain(&line, Some(&fallback)) {
                self.retain_line(PageLine {
                    text: line,
                    fallback: Some(fallback),
                });
            } else if fallback != GENERIC_OMISSION
                && self.can_retain(&fallback, Some(GENERIC_OMISSION))
            {
                self.retain_line(PageLine {
                    text: fallback,
                    fallback: Some(GENERIC_OMISSION.to_owned()),
                });
            } else if self.can_retain(GENERIC_OMISSION, None) {
                self.retain_line(PageLine {
                    text: GENERIC_OMISSION.to_owned(),
                    fallback: None,
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

    fn can_retain(&self, text: &str, fallback: Option<&str>) -> bool {
        let charge = std::mem::size_of::<PageLine>()
            .saturating_add(text.len())
            .saturating_add(fallback.map_or(0, str::len));
        self.charged.saturating_add(charge) <= PAGE_MEMORY_BYTES
    }

    fn retain_line(&mut self, line: PageLine) {
        self.charged = self.charged.saturating_add(line.charge());
        self.lines.push(line);
    }

    fn combined_skips(&self) -> SkipNotes {
        let mut notes = self.skips.clone();
        notes.merge(&self.traversal.skips);
        notes
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
    let available = page.lines.len().min(limit);
    let limits = OutputLimits::for_content_parts_within(
        page.lines
            .iter()
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
        for line in page.lines.iter().take(cap) {
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
            && let Some(fallback) = page.lines[0].fallback.as_deref()
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
