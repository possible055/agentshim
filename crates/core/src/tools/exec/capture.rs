use std::path::Path;

#[cfg(windows)]
use std::io::{self, Read, Write};

use tokio_util::sync::CancellationToken;
#[cfg(windows)]
use windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED;

use super::ProcessError;

pub const DRAIN_CHUNK_BYTES: usize = 64 * 1024;
const DIAGNOSTIC_PATH_BYTES: usize = 2 * 1024;
const DIAGNOSTIC_PATH_MARKER: &str = "...[path truncated]...";
const CAPTURE_BUDGET_FACTOR: usize = 2;

/// Raw bytes retained for one stream of a call. The budget is a per-call total divided by the
/// number of streams that call captures, so a single merged stream retains as much as a
/// two-stream call retains in aggregate.
#[must_use]
pub fn capture_bytes_per_stream(streams: usize, page_bytes: usize) -> usize {
    page_bytes
        .saturating_mul(CAPTURE_BUDGET_FACTOR)
        .div_ceil(streams.max(1))
}

#[derive(Debug)]
pub struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    head_limit: usize,
    tail_limit: usize,
    tail_start: usize,
    encoding: CaptureEncoding,
    pub bytes_read: usize,
}

#[derive(Clone, Copy, Debug)]
enum CaptureEncoding {
    Utf8,
    #[cfg(windows)]
    WindowsOem(u32),
}

impl Capture {
    pub fn new(total_bytes: usize) -> Self {
        Self::with_encoding(total_bytes, CaptureEncoding::Utf8)
    }

    #[cfg(windows)]
    pub fn new_windows_oem(total_bytes: usize, code_page: u32) -> Self {
        Self::with_encoding(total_bytes, CaptureEncoding::WindowsOem(code_page))
    }

    fn with_encoding(total_bytes: usize, encoding: CaptureEncoding) -> Self {
        let head_limit = total_bytes.div_ceil(2).max(1);
        let tail_limit = total_bytes.saturating_sub(head_limit).max(1);
        Self {
            head: Vec::new(),
            tail: Vec::new(),
            head_limit,
            tail_limit,
            tail_start: 0,
            encoding,
            bytes_read: 0,
        }
    }

    #[cfg(test)]
    pub fn head_limit(&self) -> usize {
        self.head_limit
    }

    #[cfg(test)]
    pub fn tail_limit(&self) -> usize {
        self.tail_limit
    }

    pub fn push(&mut self, bytes: &[u8]) {
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

    pub fn retained(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

    pub fn dropped(&self) -> usize {
        self.bytes_read.saturating_sub(self.retained())
    }

    pub fn render(&self, limit: usize) -> RenderedCapture {
        let limit = limit.min(self.retained()).min(self.bytes_read);
        if limit == self.bytes_read && self.dropped() == 0 {
            let mut bytes = self.head.clone();
            bytes.extend_from_slice(&self.ordered_tail());
            let (text, invalid_bytes, encoding) = self.render_bytes(&bytes);
            return RenderedCapture {
                text,
                shown_bytes: self.bytes_read,
                omitted_bytes: 0,
                invalid_bytes,
                encoding,
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
                .min(self.head_limit.saturating_add(self.tail_limit)),
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
        let (text, invalid_bytes, encoding) = self.render_bytes(&bytes);
        RenderedCapture {
            text,
            shown_bytes,
            omitted_bytes,
            invalid_bytes,
            encoding,
        }
    }

    fn render_bytes(&self, bytes: &[u8]) -> (String, usize, String) {
        let (escaped, invalid_bytes) = escape_invalid_utf8(bytes);
        if invalid_bytes == 0 {
            return (escaped, 0, "utf-8".to_owned());
        }
        #[cfg(windows)]
        if let CaptureEncoding::WindowsOem(code_page) = self.encoding
            && let Some(decoded) = decode_mixed_utf8_oem(bytes, code_page)
        {
            return (
                decoded,
                invalid_bytes,
                format!("windows-oem-{code_page}-fallback"),
            );
        }
        let _ = self.encoding;
        (escaped, invalid_bytes, "utf-8-with-byte-escapes".to_owned())
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

pub struct RenderedCapture {
    pub text: String,
    pub shown_bytes: usize,
    pub omitted_bytes: usize,
    pub invalid_bytes: usize,
    pub encoding: String,
}

pub fn push_output_line(output: &mut String, line: &str) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(line);
}

pub fn push_capture_section(output: &mut String, label: &str, capture: &RenderedCapture) {
    if capture.text.is_empty() {
        return;
    }
    push_output_line(output, &format!("--- {label} ---"));
    push_output_line(output, &capture.text);
}

pub fn push_capture_diagnostics(
    output: &mut String,
    label: &str,
    total_bytes: usize,
    capture: &RenderedCapture,
) {
    if capture.omitted_bytes > 0 {
        push_output_line(
            output,
            &format!(
                "{label}: total={total_bytes} shown={} omitted={}.",
                capture.shown_bytes, capture.omitted_bytes
            ),
        );
    }
    if capture.invalid_bytes > 0 || capture.encoding != "utf-8" {
        push_output_line(
            output,
            &format!(
                "{label} encoding: invalid={} encoding={}.",
                capture.invalid_bytes, capture.encoding
            ),
        );
    }
}

#[cfg(windows)]
fn decode_mixed_utf8_oem(mut bytes: &[u8], code_page: u32) -> Option<String> {
    use windows_sys::Win32::Globalization::IsDBCSLeadByteEx;

    let mut output = String::new();
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                output.push_str(text);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                output.push_str(std::str::from_utf8(&bytes[..valid]).ok()?);
                let invalid = &bytes[valid..];
                let fallback_len = if invalid.len() >= 2
                    && unsafe { IsDBCSLeadByteEx(code_page, invalid[0]) } != 0
                {
                    2
                } else {
                    error.error_len().unwrap_or(invalid.len())
                };
                output.push_str(&decode_windows_code_page(
                    &invalid[..fallback_len],
                    code_page,
                )?);
                bytes = &invalid[fallback_len..];
            }
        }
    }
    Some(output)
}

#[cfg(windows)]
fn decode_windows_code_page(bytes: &[u8], code_page: u32) -> Option<String> {
    use windows_sys::Win32::Globalization::{MB_ERR_INVALID_CHARS, MultiByteToWideChar};

    let input_len = i32::try_from(bytes.len()).ok()?;
    let required = unsafe {
        MultiByteToWideChar(
            code_page,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            input_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if required == 0 {
        return None;
    }
    let mut wide = vec![0_u16; usize::try_from(required).ok()?];
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            input_len,
            wide.as_mut_ptr(),
            required,
        )
    };
    if written != required {
        return None;
    }
    String::from_utf16(&wide).ok()
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

pub fn escape_invalid_utf8(bytes: &[u8]) -> (String, usize) {
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

#[cfg(all(windows, test))]
pub fn drain(
    mut reader: impl Read,
    capture_bytes: usize,
    oem_code_page: Option<u32>,
) -> io::Result<Capture> {
    drain_with_capture(&mut reader, capture_bytes, oem_code_page, None, 0)
}

#[cfg(windows)]
pub fn drain_with_capture(
    mut reader: impl Read,
    capture_bytes: usize,
    oem_code_page: Option<u32>,
    capture_sink: Option<&dyn super::spawn::CaptureSink>,
    stream: usize,
) -> io::Result<Capture> {
    let mut capture = oem_code_page.map_or_else(
        || Capture::new(capture_bytes),
        |code_page| Capture::new_windows_oem(capture_bytes, code_page),
    );
    let mut chunk = vec![0_u8; DRAIN_CHUNK_BYTES].into_boxed_slice();
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(count) => count,
            Err(error) if is_operation_aborted(&error) => {
                return Ok(capture);
            }
            Err(error) => return Err(error),
        };
        if count == 0 {
            return Ok(capture);
        }
        if let Some(sink) = capture_sink {
            sink.append(stream, &chunk[..count])?;
        }
        capture.push(&chunk[..count]);
    }
}

#[cfg(windows)]
pub fn write_stdin(mut writer: impl Write, input: Option<&str>) -> io::Result<()> {
    if let Some(input) = input {
        if let Err(error) = writer.write_all(input.as_bytes()) {
            if !is_operation_aborted(&error) {
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_operation_aborted(error: &io::Error) -> bool {
    error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
        == Some(ERROR_OPERATION_ABORTED)
}

/// Grow every capture together, then grow each one individually, until the assembled result
/// stops fitting. Streams with more retained bytes expand first so a large stream cannot be
/// starved by a small one that has already been shown in full.
pub fn project_captures<T>(
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

pub fn diagnostic_path(path: &Path) -> String {
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
