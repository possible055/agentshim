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

#[derive(Debug)]
pub(super) struct LineCollector {
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
}

impl LineCollector {
    pub(super) fn new(start: usize, line_count: Option<usize>) -> Self {
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
        }
    }

    pub(super) fn push(&mut self, text: &str) -> DecodeControl {
        self.saw_input |= !text.is_empty();
        let mut remaining = text;
        while let Some(newline) = remaining.as_bytes().iter().position(|byte| *byte == b'\n') {
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
        for character in text.chars() {
            if self.current.len() >= LINE_PREFIX_BYTES {
                break;
            }
            self.current.push(character);
        }
    }

    pub(super) fn finish_eof(&mut self) {
        if !self.stopped && self.saw_input && !self.ended_with_newline {
            self.stopped = !self.finish_line();
        }
    }

    fn finish_line(&mut self) -> bool {
        if self.current.ends_with('\r') {
            self.current.pop();
            self.current_bytes = self.current_bytes.saturating_sub(1);
        }
        if self.current_number >= self.start {
            if self.candidates.len() > self.requested {
                return false;
            }
            let stored_bytes = self.current.len();
            if self.candidate_bytes.saturating_add(stored_bytes) > CANDIDATE_BYTES
                && !self.candidates.is_empty()
            {
                return false;
            }
            self.candidate_bytes = self.candidate_bytes.saturating_add(stored_bytes);
            self.candidates.push(CandidateLine {
                number: self.current_number,
                prefix: std::mem::take(&mut self.current),
                truncated: self.current_bytes > stored_bytes,
            });
            if self.candidates.len() > self.requested {
                return false;
            }
        }
        self.current_number = self.current_number.saturating_add(1);
        self.current.clear();
        self.current_bytes = 0;
        true
    }
}

pub(super) fn render(
    absolute: &str,
    request: &ReadRequest,
    source_encoding: SourceEncoding,
    collector: &LineCollector,
    cancellation: &CancellationToken,
    output_budget: &crate::output::CallOutputBudget,
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
    let limits = OutputLimits::for_content_parts(
        collector
            .candidates
            .iter()
            .take(available)
            .map(|line| line.prefix.as_str()),
    );
    let mut cap = available;
    loop {
        let partial = source_has_more || cap < available;
        let next_start_line = partial.then(|| collector.start.saturating_add(cap));
        let tail = next_start_line
            .map(|next| format!("Partial: next_start_line={next}."))
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
