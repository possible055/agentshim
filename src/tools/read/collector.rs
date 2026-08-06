#[derive(Debug)]
struct CandidateLine {
    number: usize,
    prefix: String,
    truncated: bool,
}

#[derive(Debug)]
struct LineCollector {
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
    fn new(start: usize, line_count: Option<usize>) -> Self {
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

    fn push(&mut self, text: &str) -> DecodeControl {
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
        if self.current.len() >= LINE_PREFIX_BYTES {
            return;
        }
        for character in text.chars() {
            if self.current.len() >= LINE_PREFIX_BYTES {
                break;
            }
            self.current.push(character);
        }
    }

    fn finish_eof(&mut self) {
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

fn render(
    absolute: &str,
    request: &ReadRequest,
    source_encoding: SourceEncoding,
    collector: &LineCollector,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ReadError> {
    let available = collector.candidates.len().min(collector.requested);
    let source_has_more = collector.stopped || collector.candidates.len() > available;
    let header = if source_encoding == SourceEncoding::Utf8 {
        format!("Path: {absolute}")
    } else {
        format!("Path: {absolute}\nEncoding: {}", source_encoding.name())
    };
    let mut cap = available;
    loop {
        let partial = source_has_more || cap < available;
        let next_start_line = partial.then(|| collector.start.saturating_add(cap));
        let tail = next_start_line.map_or_else(
            || "Complete.".to_owned(),
            |next| format!("Partial: next_start_line={next}."),
        );
        let mut formatter =
            OutputFormatter::new(header.clone(), vec![tail], OutputLimits::default())?;
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
        let lines = collector
            .candidates
            .iter()
            .take(cap)
            .map(|line| ReadLine {
                number: line.number,
                text: line.prefix.trim_end_matches('\r').to_owned(),
                truncated: line.truncated,
            })
            .collect::<Vec<_>>();
        let result = ReadResult {
            path: absolute.to_owned(),
            encoding: source_encoding.name().to_owned(),
            start_line: collector.start,
            lines,
            next_start_line,
            complete: next_start_line.is_none(),
        };
        let output = ToolOutput::new(formatter.finish(cancellation)?, &result)?;
        if output.fits_budget() {
            let _ = request;
            return Ok(output);
        }
        if cap == 0 {
            return Err(crate::output::OutputError::NoProgress.into());
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


#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileFingerprint {
    pub(crate) regular: bool,
    platform: PlatformFingerprint,
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformFingerprint {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(windows)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformFingerprint {
    volume: u64,
    file_id: [u8; 16],
    length: i64,
    last_write_time: i64,
    change_time: i64,
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformFingerprint {
    length: u64,
    modified: Option<std::time::SystemTime>,
}

impl FileFingerprint {
    #[cfg(unix)]
    pub(crate) fn from_file(file: &File) -> io::Result<Self> {
        use cap_std::fs::MetadataExt;

        let metadata = file.metadata()?;
        Ok(Self {
            regular: metadata.is_file(),
            platform: PlatformFingerprint {
                device: metadata.dev(),
                inode: metadata.ino(),
                length: metadata.len(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                changed_seconds: metadata.ctime(),
                changed_nanoseconds: metadata.ctime_nsec(),
            },
        })
    }

    #[cfg(windows)]
    pub(crate) fn from_file(file: &File) -> io::Result<Self> {
        use std::{mem::size_of, os::windows::io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FILE_ID_INFO, FILE_STANDARD_INFO, FileBasicInfo, FileIdInfo,
            FileStandardInfo, GetFileInformationByHandleEx,
        };

        fn query<T: Default>(
            handle: windows_sys::Win32::Foundation::HANDLE,
            class: i32,
        ) -> io::Result<T> {
            let mut value = T::default();
            let size = u32::try_from(size_of::<T>()).expect("file information size fits DWORD");
            // SAFETY: `handle` is borrowed from a live file, and `value` is a writable buffer of
            // exactly the structure size corresponding to `class` at each call site.
            let succeeded = unsafe {
                GetFileInformationByHandleEx(handle, class, (&raw mut value).cast(), size)
            };
            if succeeded == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(value)
            }
        }

        let handle = file.as_raw_handle();
        let id: FILE_ID_INFO = query(handle, FileIdInfo)?;
        let standard: FILE_STANDARD_INFO = query(handle, FileStandardInfo)?;
        let basic: FILE_BASIC_INFO = query(handle, FileBasicInfo)?;
        Ok(Self {
            regular: !standard.Directory,
            platform: PlatformFingerprint {
                volume: id.VolumeSerialNumber,
                file_id: id.FileId.Identifier,
                length: standard.EndOfFile,
                last_write_time: basic.LastWriteTime,
                change_time: basic.ChangeTime,
            },
        })
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn from_file(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            regular: metadata.is_file(),
            platform: PlatformFingerprint {
                length: metadata.len(),
                modified: metadata.modified().ok().map(Into::into),
            },
        })
    }
}

#[cfg(test)]
thread_local! {
    static BEFORE_READ_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_READ_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_before_read_hook() {
    BEFORE_READ_HOOK.with(|hook| {
        if let Some(mut hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn run_before_read_hook() {}

#[cfg(test)]
fn run_after_read_hook() {
    AFTER_READ_HOOK.with(|hook| {
        if let Some(mut hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn run_after_read_hook() {}

include!("tests.rs");
