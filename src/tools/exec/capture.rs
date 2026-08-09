use std::path::Path;

#[cfg(windows)]
use std::io::{self, Read, Write};

use tokio_util::sync::CancellationToken;

use super::ProcessError;

pub(super) const DRAIN_CHUNK_BYTES: usize = 64 * 1024;
const DIAGNOSTIC_PATH_BYTES: usize = 2 * 1024;
const DIAGNOSTIC_PATH_MARKER: &str = "...[path truncated]...";
const CAPTURE_BUDGET_FACTOR: usize = 2;

/// Raw bytes retained for one stream of a call. The budget is a per-call total divided by the
/// number of streams that call captures, so a single merged stream retains as much as a
/// two-stream call retains in aggregate.
#[must_use]
pub(crate) fn capture_bytes_per_stream(streams: usize) -> usize {
    crate::output::effective_byte_limit()
        .saturating_mul(CAPTURE_BUDGET_FACTOR)
        .div_ceil(streams.max(1))
}

#[derive(Debug)]
pub(crate) struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    head_limit: usize,
    tail_limit: usize,
    tail_start: usize,
    pub(crate) bytes_read: usize,
}

impl Capture {
    pub(crate) fn new(total_bytes: usize) -> Self {
        let head_limit = total_bytes.div_ceil(2).max(1);
        let tail_limit = total_bytes.saturating_sub(head_limit).max(1);
        Self {
            head: Vec::new(),
            tail: Vec::new(),
            head_limit,
            tail_limit,
            tail_start: 0,
            bytes_read: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn head_limit(&self) -> usize {
        self.head_limit
    }

    #[cfg(test)]
    pub(super) fn tail_limit(&self) -> usize {
        self.tail_limit
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.bytes_read = self.bytes_read.saturating_add(bytes.len());
        let head_remaining = self.head_limit.saturating_sub(self.head.len());
        let head_bytes = head_remaining.min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_bytes]);
        self.push_tail(&bytes[head_bytes..]);
    }

    fn push_tail(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.tail_limit {
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len() - self.tail_limit..]);
            self.tail_start = 0;
            return;
        }
        if self.tail.len() < self.tail_limit {
            let appended = bytes.len().min(self.tail_limit - self.tail.len());
            self.tail.extend_from_slice(&bytes[..appended]);
            if appended == bytes.len() {
                return;
            }
            self.overwrite_tail(&bytes[appended..]);
            return;
        }
        self.overwrite_tail(bytes);
    }

    fn overwrite_tail(&mut self, bytes: &[u8]) {
        let first = bytes.len().min(self.tail_limit - self.tail_start);
        self.tail[self.tail_start..self.tail_start + first].copy_from_slice(&bytes[..first]);
        self.tail[..bytes.len() - first].copy_from_slice(&bytes[first..]);
        self.tail_start = (self.tail_start + bytes.len()) % self.tail_limit;
    }

    pub(crate) fn retained(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

    pub(super) fn dropped(&self) -> usize {
        self.bytes_read.saturating_sub(self.retained())
    }

    pub(crate) fn render(&self, limit: usize) -> RenderedCapture {
        let limit = limit.min(self.retained()).min(self.bytes_read);
        if limit == self.bytes_read && self.dropped() == 0 {
            let mut bytes = self.head.clone();
            bytes.extend_from_slice(&self.ordered_tail());
            let (text, invalid_bytes) = escape_invalid_utf8(&bytes);
            return RenderedCapture {
                text,
                shown_bytes: self.bytes_read,
                omitted_bytes: 0,
                invalid_bytes,
            };
        }

        let ordered_tail = self.ordered_tail();
        let contiguous;
        let (head_source, tail_source) = if self.dropped() == 0 {
            contiguous = {
                let mut bytes = self.head.clone();
                bytes.extend_from_slice(&ordered_tail);
                bytes
            };
            (contiguous.as_slice(), contiguous.as_slice())
        } else {
            (self.head.as_slice(), ordered_tail.as_slice())
        };
        let (head_count, tail_count) =
            allocate_view_bytes(limit, head_source.len(), tail_source.len());
        let head = align_head(&head_source[..head_count]);
        let tail = align_tail(&tail_source[tail_source.len().saturating_sub(tail_count)..]);
        let shown_bytes = head.len().saturating_add(tail.len());
        let omitted_bytes = self.bytes_read.saturating_sub(shown_bytes);
        let mut bytes = Vec::with_capacity(
            shown_bytes
                .saturating_add(64)
                .min(crate::output::effective_byte_limit()),
        );
        bytes.extend_from_slice(head);
        if omitted_bytes > 0 {
            if bytes.last().is_some_and(|byte| *byte != b'\n') {
                bytes.push(b'\n');
            }
            bytes.extend_from_slice(format!("... {omitted_bytes} bytes omitted ...").as_bytes());
            if !tail.is_empty() && tail.first().is_some_and(|byte| *byte != b'\n') {
                bytes.push(b'\n');
            }
        }
        bytes.extend_from_slice(tail);
        let (text, invalid_bytes) = escape_invalid_utf8(&bytes);
        RenderedCapture {
            text,
            shown_bytes,
            omitted_bytes,
            invalid_bytes,
        }
    }

    fn ordered_tail(&self) -> Vec<u8> {
        if self.tail.len() < self.tail_limit || self.tail_start == 0 {
            return self.tail.clone();
        }
        let mut ordered = Vec::with_capacity(self.tail.len());
        ordered.extend_from_slice(&self.tail[self.tail_start..]);
        ordered.extend_from_slice(&self.tail[..self.tail_start]);
        ordered
    }
}

pub(crate) struct RenderedCapture {
    pub(crate) text: String,
    pub(crate) shown_bytes: usize,
    pub(crate) omitted_bytes: usize,
    pub(crate) invalid_bytes: usize,
}

fn allocate_view_bytes(
    limit: usize,
    head_available: usize,
    tail_available: usize,
) -> (usize, usize) {
    let mut head = limit.div_ceil(2).min(head_available);
    let mut tail = (limit / 2).min(tail_available);
    let mut remaining = limit.saturating_sub(head).saturating_sub(tail);
    let extra_head = remaining.min(head_available.saturating_sub(head));
    head += extra_head;
    remaining -= extra_head;
    tail += remaining.min(tail_available.saturating_sub(tail));
    (head, tail)
}

fn align_head(bytes: &[u8]) -> &[u8] {
    let clipped = &bytes[..trim_incomplete_utf8_suffix(bytes)];
    if let Some(end) = clipped.iter().rposition(|byte| *byte == b'\n') {
        let aligned = &clipped[..=end];
        if aligned.len() >= clipped.len().div_ceil(2) {
            return aligned;
        }
    }
    clipped
}

fn align_tail(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && is_utf8_continuation(bytes[start]) {
        start += 1;
    }
    let clipped = &bytes[start..];
    if let Some(end) = clipped.iter().position(|byte| *byte == b'\n') {
        let aligned = &clipped[end + 1..];
        if aligned.len() >= clipped.len().div_ceil(2) {
            return aligned;
        }
    }
    clipped
}

fn trim_incomplete_utf8_suffix(bytes: &[u8]) -> usize {
    let end = bytes.len();
    let mut lead = end;
    while lead > 0 && is_utf8_continuation(bytes[lead - 1]) && end - lead < 3 {
        lead -= 1;
    }
    if lead == 0 {
        return end;
    }
    let first = bytes[lead - 1];
    let width = match first {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return end,
    };
    let sequence_start = lead - 1;
    if end - sequence_start < width {
        sequence_start
    } else {
        end
    }
}

fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

pub(crate) fn escape_invalid_utf8(bytes: &[u8]) -> (String, usize) {
    let mut input = bytes;
    let mut output = String::new();
    let mut invalid = 0_usize;
    while !input.is_empty() {
        match std::str::from_utf8(input) {
            Ok(text) => {
                output.push_str(text);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                output.push_str(
                    std::str::from_utf8(&input[..valid])
                        .expect("valid_up_to always identifies valid UTF-8"),
                );
                let count = error
                    .error_len()
                    .unwrap_or(input.len().saturating_sub(valid));
                for byte in &input[valid..valid + count] {
                    use std::fmt::Write as _;
                    let _ = write!(output, "\\x{byte:02X}");
                    invalid += 1;
                }
                input = &input[valid + count..];
            }
        }
    }
    (output, invalid)
}

#[cfg(windows)]
pub(super) fn drain(mut reader: impl Read, capture_bytes: usize) -> io::Result<Capture> {
    let mut capture = Capture::new(capture_bytes);
    let mut chunk = vec![0_u8; DRAIN_CHUNK_BYTES].into_boxed_slice();
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(capture);
        }
        capture.push(&chunk[..count]);
    }
}

#[cfg(windows)]
pub(super) fn write_stdin(mut writer: impl Write, input: Option<&str>) -> io::Result<()> {
    if let Some(input) = input {
        writer.write_all(input.as_bytes())?;
    }
    Ok(())
}

/// Grow every capture together, then grow each one individually, until the assembled result
/// stops fitting. Streams with more retained bytes expand first so a large stream cannot be
/// starved by a small one that has already been shown in full.
pub(crate) fn project_captures<T>(
    captures: &[&Capture],
    cancellation: &CancellationToken,
    mut build: impl FnMut(&[RenderedCapture]) -> T,
    mut fits: impl FnMut(&T) -> bool,
) -> Result<T, ProcessError> {
    let maximum = captures
        .iter()
        .map(|capture| capture.retained())
        .collect::<Vec<_>>();
    let full = build_capture_candidate(captures, &maximum, &mut build);
    if fits(&full) {
        return Ok(full);
    }
    check_render_cancellation(cancellation)?;

    let empty = vec![0_usize; captures.len()];
    let minimal = build_capture_candidate(captures, &empty, &mut build);
    if !fits(&minimal) {
        return Err(crate::output::OutputError::RequiredContentTooLarge.into());
    }

    let mut low = 0_usize;
    let mut high = maximum.iter().copied().max().unwrap_or(0).saturating_add(1);
    while low + 1 < high {
        check_render_cancellation(cancellation)?;
        let midpoint = low + (high - low) / 2;
        let quotas = maximum
            .iter()
            .map(|available| midpoint.min(*available))
            .collect::<Vec<_>>();
        let candidate = build_capture_candidate(captures, &quotas, &mut build);
        if fits(&candidate) {
            low = midpoint;
        } else {
            high = midpoint;
        }
    }

    let mut quotas = maximum
        .iter()
        .map(|available| low.min(*available))
        .collect::<Vec<_>>();
    let mut order = (0..captures.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| std::cmp::Reverse(maximum[*index].saturating_sub(quotas[*index])));
    for index in order {
        let mut low = quotas[index];
        let mut high = maximum[index].saturating_add(1);
        while low + 1 < high {
            check_render_cancellation(cancellation)?;
            let midpoint = low + (high - low) / 2;
            let mut candidate_quotas = quotas.clone();
            candidate_quotas[index] = midpoint;
            let candidate = build_capture_candidate(captures, &candidate_quotas, &mut build);
            if fits(&candidate) {
                low = midpoint;
            } else {
                high = midpoint;
            }
        }
        quotas[index] = low;
    }

    let candidate = build_capture_candidate(captures, &quotas, &mut build);
    if fits(&candidate) {
        return Ok(candidate);
    }
    Ok(minimal)
}

fn build_capture_candidate<T>(
    captures: &[&Capture],
    quotas: &[usize],
    build: &mut impl FnMut(&[RenderedCapture]) -> T,
) -> T {
    let rendered = captures
        .iter()
        .zip(quotas)
        .map(|(capture, quota)| capture.render(*quota))
        .collect::<Vec<_>>();
    build(&rendered)
}

fn check_render_cancellation(cancellation: &CancellationToken) -> Result<(), ProcessError> {
    if cancellation.is_cancelled() {
        return Err(crate::output::OutputError::Cancelled.into());
    }
    Ok(())
}

pub(crate) fn diagnostic_path(path: &Path) -> String {
    let rendered = path.display().to_string();
    if rendered.len() <= DIAGNOSTIC_PATH_BYTES {
        return rendered;
    }
    let retained = DIAGNOSTIC_PATH_BYTES - DIAGNOSTIC_PATH_MARKER.len();
    let mut head_end = retained / 2;
    while !rendered.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = rendered.len() - (retained - head_end);
    while !rendered.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}{}{}",
        &rendered[..head_end],
        DIAGNOSTIC_PATH_MARKER,
        &rendered[tail_start..]
    )
}
