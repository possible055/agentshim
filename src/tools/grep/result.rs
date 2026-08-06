struct PageLine {
    text: String,
    fallback: Option<String>,
    item: GrepItem,
}

impl PageLine {
    fn charge(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.text.len())
            .saturating_add(self.fallback.as_deref().map_or(0, str::len))
    }
}

struct Page {
    lines: Vec<PageLine>,
    total: usize,
    skipped: usize,
    offset: usize,
    retain: usize,
    charged: usize,
    retaining: bool,
    traversal: TraversalSummary,
}

impl Page {
    fn new(request: &GrepRequest, traversal: TraversalSummary) -> Self {
        Self {
            lines: Vec::new(),
            total: 0,
            skipped: 0,
            offset: request.offset.unwrap_or(0),
            retain: request.limit.unwrap_or(DEFAULT_LIMIT).saturating_add(1),
            charged: 0,
            retaining: true,
            traversal,
        }
    }

    fn reduce(
        &mut self,
        outcome: FileOutcome,
        mode: GrepMode,
        single_file: bool,
    ) -> Result<(), GrepError> {
        if outcome.skipped {
            self.skipped = self.skipped.saturating_add(1);
            if single_file {
                return Err(GrepError::Io(io::Error::other(
                    "single grep target changed, is binary, or could not be searched",
                )));
            }
            return Ok(());
        }
        match mode {
            GrepMode::Files if outcome.matched => {
                let path = outcome.absolute;
                self.push_typed_entry(
                    path.clone(),
                    None,
                    GrepItem {
                        kind: "file",
                        path: Some(path),
                        line_number: None,
                        text: None,
                        matched: None,
                        count: None,
                    },
                );
            }
            GrepMode::Count if outcome.matched => {
                let path = outcome.absolute;
                let count = outcome.occurrences;
                self.push_typed_entry(
                    format!("{path}:{count}"),
                    None,
                    GrepItem {
                        kind: "count",
                        path: Some(path),
                        line_number: None,
                        text: None,
                        matched: None,
                        count: Some(u64::try_from(count).unwrap_or(u64::MAX)),
                    },
                );
            }
            GrepMode::Content => {
                let entries = outcome.entries;
                let captured = outcome.records.len();
                for record in outcome.records {
                    let separator = if record.kind == RecordKind::Match {
                        ':'
                    } else {
                        '-'
                    };
                    let fallback = format!(
                        "{}{separator}{}{separator}{CONTENT_OMISSION}",
                        outcome.absolute, record.line
                    );
                    self.push_typed_entry(
                        format!(
                            "{}{separator}{}{separator}{}",
                            outcome.absolute, record.line, record.text
                        ),
                        Some(fallback),
                        GrepItem {
                            kind: "content",
                            path: Some(outcome.absolute.clone()),
                            line_number: Some(record.line),
                            text: Some(record.text),
                            matched: Some(record.kind == RecordKind::Match),
                            count: None,
                        },
                    );
                }
                self.total = self.total.saturating_add(entries.saturating_sub(captured));
            }
            GrepMode::Files | GrepMode::Count => {}
        }
        Ok(())
    }

    #[cfg(test)]
    fn push_entry(&mut self, line: String, detailed_fallback: Option<String>) {
        let item = GrepItem {
            kind: "text",
            path: None,
            line_number: None,
            text: Some(line.clone()),
            matched: None,
            count: None,
        };
        self.push_typed_entry(line, detailed_fallback, item);
    }

    fn push_typed_entry(
        &mut self,
        line: String,
        detailed_fallback: Option<String>,
        item: GrepItem,
    ) {
        if self.total >= self.offset && self.lines.len() < self.retain && self.retaining {
            let fallback = detailed_fallback.unwrap_or_else(|| GENERIC_OMISSION.to_owned());
            if self.can_retain(&line, Some(&fallback)) {
                self.retain_line(PageLine {
                    text: line,
                    fallback: Some(fallback),
                    item,
                });
            } else if fallback != GENERIC_OMISSION
                && self.can_retain(&fallback, Some(GENERIC_OMISSION))
            {
                self.retain_line(PageLine {
                    text: fallback,
                    fallback: Some(GENERIC_OMISSION.to_owned()),
                    item,
                });
            } else if self.can_retain(GENERIC_OMISSION, None) {
                self.retain_line(PageLine {
                    text: GENERIC_OMISSION.to_owned(),
                    fallback: None,
                    item,
                });
            } else {
                self.retaining = false;
            }
        }
        self.total = self.total.saturating_add(1);
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
}

fn pagination_tail(total_skipped: usize, next_offset: Option<usize>) -> Vec<String> {
    let mut tail = Vec::new();
    if total_skipped > 0 {
        tail.push(format!("Skipped: {total_skipped} files or entries."));
    }
    tail.push(next_offset.map_or_else(
        || "Complete.".to_owned(),
        |next| format!("Partial: next_offset={next}."),
    ));
    tail
}

fn render(
    request: &GrepRequest,
    page: &Page,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, GrepError> {
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let available = page.lines.len().min(limit);
    let mut cap = available;
    loop {
        let total_skipped = page.skipped.saturating_add(page.traversal.skipped());
        let next_offset = (page.offset.saturating_add(cap) < page.total)
            .then(|| page.offset.saturating_add(cap));
        let tail = pagination_tail(total_skipped, next_offset);
        let mut formatter = OutputFormatter::new(
            format!("Pattern: {}", request.pattern),
            tail,
            OutputLimits::default(),
        )?;
        let mut items = Vec::with_capacity(cap);
        for line in page.lines.iter().take(cap) {
            if formatter.try_push_line(&line.text, cancellation)? {
                items.push(line.item.clone());
                continue;
            }
            if items.is_empty()
                && let Some(fallback) = line.fallback.as_deref()
                && formatter.try_push_line(fallback, cancellation)?
            {
                items.push(GrepItem {
                    kind: "omitted",
                    path: None,
                    line_number: None,
                    text: Some(fallback.to_owned()),
                    matched: None,
                    count: None,
                });
            }
            break;
        }
        if items.len() < cap {
            cap = items.len();
            continue;
        }
        let result = GrepResult {
            mode: request.mode.unwrap_or_default(),
            items,
            total: page.total,
            offset: page.offset,
            limit,
            next_offset,
            skipped: total_skipped,
            traversal: page.traversal,
        };
        let output = ToolOutput::new(formatter.finish(cancellation)?, &result)?;
        if output.fits_budget() {
            return Ok(output);
        }
        if cap == 1
            && let Some(fallback) = page.lines[0].fallback.as_deref()
        {
            let tail = pagination_tail(total_skipped, next_offset);
            let mut formatter = OutputFormatter::new(
                format!("Pattern: {}", request.pattern),
                tail,
                OutputLimits::default(),
            )?;
            if !formatter.try_push_line(fallback, cancellation)? {
                return Err(crate::output::OutputError::NoProgress.into());
            }
            let fallback_result = GrepResult {
                mode: request.mode.unwrap_or_default(),
                items: vec![GrepItem {
                    kind: "omitted",
                    path: None,
                    line_number: None,
                    text: Some(fallback.to_owned()),
                    matched: None,
                    count: None,
                }],
                total: page.total,
                offset: page.offset,
                limit,
                next_offset,
                skipped: total_skipped,
                traversal: page.traversal,
            };
            let fallback_output =
                ToolOutput::new(formatter.finish(cancellation)?, &fallback_result)?;
            if fallback_output.fits_budget() {
                return Ok(fallback_output);
            }
        }
        if cap == 0 {
            return Err(crate::output::OutputError::NoProgress.into());
        }
        cap -= 1;
    }
}

include!("tests.rs");
