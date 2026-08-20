use memchr::memchr;
use tokio_util::sync::CancellationToken;

use crate::{
    encoding::{DecodeControl, SourceEncoding},
    output::{OutputFormatter, OutputLimits},
    tools::ToolOutput,
};

use super::request::{CANDIDATE_BYTES, LINE_PREFIX_BYTES, ReadError, ReadRequest};

#[derive(Debug)]
struct CandidateLine {
    number: usize,
    prefix: String,
    truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CollectStop {
    None,
    LineCount,
    CandidateBudget,
}

#[derive(Debug)]
pub struct LineCollector {
    start: usize,
    requested: usize,
    current_number: usize,
    current: String,
    current_bytes: usize,
    candidate_bytes: usize,
    candidates: Vec<CandidateLine>,
    saw_input: bool,
    ended_with_newline: bool,
    stopped: bool,
    stop: CollectStop,
}

impl LineCollector {
    pub fn new(start: usize, line_count: Option<usize>) -> Self {
        Self {
            start,
            requested: line_count.unwrap_or(usize::MAX),
            current_number: 1,
            current: String::new(),
            current_bytes: 0,
            candidate_bytes: 0,
            candidates: Vec::new(),
            saw_input: false,
            ended_with_newline: false,
            stopped: false,
            stop: CollectStop::None,
        }
    }

    pub fn push(&mut self, text: &str) -> DecodeControl {
        self.saw_input |= !text.is_empty();
        let mut remaining = text;
        while let Some(newline) = memchr(b'\n', remaining.as_bytes()) {
            self.push_segment(&remaining[..newline]);
            self.ended_with_newline = true;
            if !self.finish_line() {
                self.stopped = true;
                return DecodeControl::Stop;
            }
            remaining = &remaining[newline + 1..];
        }
        self.push_segment(remaining);
        if !remaining.is_empty() {
            self.ended_with_newline = false;
        }
        DecodeControl::Continue
    }

    fn push_segment(&mut self, text: &str) {
        self.current_bytes = self.current_bytes.saturating_add(text.len());
        if self.current_number < self.start || self.current.len() >= LINE_PREFIX_BYTES {
            return;
        }
        let remaining = LINE_PREFIX_BYTES.saturating_sub(self.current.len());
        let bytes = text.as_bytes();
        let mut end = bytes.len().min(remaining);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        self.current.push_str(&text[..end]);
    }

    pub fn finish_eof(&mut self) {
        if !self.stopped && self.saw_input && !self.ended_with_newline {
            self.stopped = !self.finish_line();
        }
    }

    #[cfg(test)]
    pub fn allocation_state_for_test(&self) -> (usize, usize) {
        (self.current.capacity(), self.candidates.capacity())
    }

    fn finish_line(&mut self) -> bool {
        if self.current.ends_with('\r') {
            self.current.pop();
            self.current_bytes = self.current_bytes.saturating_sub(1);
        }
        if self.current_number >= self.start {
            if self.candidates.len() > self.requested {
                self.stop = CollectStop::LineCount;
                return false;
            }
            let stored_bytes = self.current.len();
            if self.candidate_bytes.saturating_add(self.current_bytes) > CANDIDATE_BYTES
                && !self.candidates.is_empty()
            {
                self.stop = CollectStop::CandidateBudget;
                return false;
            }
            self.candidate_bytes = self.candidate_bytes.saturating_add(self.current_bytes);
            self.candidates.push(CandidateLine {
                number: self.current_number,
                prefix: std::mem::take(&mut self.current),
                truncated: self.current_bytes > stored_bytes,
            });
            if self.candidates.len() > self.requested {
                self.stop = CollectStop::LineCount;
                return false;
            }
        }
        self.current_number = self.current_number.saturating_add(1);
        self.current.clear();
        self.current_bytes = 0;
        true
    }
}

pub fn render(
    absolute: &str,
    request: &ReadRequest,
    source_encoding: SourceEncoding,
    collector: &LineCollector,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<ToolOutput, ReadError> {
    let available = collector.candidates.len().min(collector.requested);
    let source_has_more = collector.stopped || collector.candidates.len() > available;
    let header = if source_encoding == SourceEncoding::Utf8 {
        String::new()
    } else {
        format!("Encoding: {}", source_encoding.name())
    };
    if available == 0 && !source_has_more {
        let message = if collector.start == 1 {
            "No lines.".to_owned()
        } else {
            format!("No lines at or after start_line={}.", collector.start)
        };
        let text = if header.is_empty() {
            message
        } else {
            format!("{header}\n{message}")
        };
        return Ok(ToolOutput::new(text));
    }
    let limits = OutputLimits::for_content_parts_within(
        collector
            .candidates
            .iter()
            .take(available)
            .map(|line| line.prefix.as_str()),
        output_budget.page_bytes(),
    );
    let mut cap = available;
    loop {
        let output_budget_stop = cap < available;
        let partial = source_has_more || output_budget_stop;
        let next_start_line = partial.then(|| collector.start.saturating_add(cap));
        let tail = next_start_line
            .map(|next| {
                format!(
                    "{} {}={next}. ({})",
                    crate::output::PARTIAL_MARKER,
                    crate::output::NEXT_START_LINE_FIELD,
                    partial_stop_reason(collector, output_budget_stop)
                )
            })
            .into_iter()
            .collect();
        let mut formatter = OutputFormatter::new(header.clone(), tail, limits)?;
        let mut shown = 0_usize;
        for line in collector.candidates.iter().take(cap) {
            if formatter.try_push_line(render_candidate(line), cancellation)? {
                shown += 1;
            } else {
                break;
            }
        }
        if shown < cap {
            cap = shown;
            continue;
        }
        let output = ToolOutput::new(formatter.finish(cancellation)?);
        if output.fits_budget_and_call(output_budget, cancellation) {
            let _ = (absolute, request);
            return Ok(output);
        }
        if cap == 0 {
            return Err(crate::output::OutputError::BurstLimit.into());
        }
        cap -= 1;
    }
}

fn partial_stop_reason(collector: &LineCollector, output_budget_stop: bool) -> &'static str {
    if output_budget_stop {
        "output budget"
    } else if collector.stop == CollectStop::CandidateBudget {
        "read candidate budget"
    } else {
        "line_count"
    }
}

fn render_candidate(line: &CandidateLine) -> String {
    if line.truncated {
        format!(
            "{}\t{}… [line truncated]",
            line.number,
            line.prefix.trim_end_matches('\r')
        )
    } else {
        format!("{}\t{}", line.number, line.prefix)
    }
}
