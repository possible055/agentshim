use std::{
    io::{self, Read},
    path::Path,
    sync::Arc,
};

use cap_std::fs::{File, OpenOptions};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    encoding::{DecodeControl, DecodeError, SourceEncoding, decode_stream},
    output::{OutputFormatter, OutputLimits},
    path::{PathError, RepositoryRoot},
};

const PREFIX_BYTES: usize = 8 * 1024;
const CANDIDATE_BYTES: usize = 64 * 1024;
const LINE_PREFIX_BYTES: usize = 16 * 1024;
const MAX_LINE_COUNT: usize = 2_000;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub path: String,
    pub start_line: Option<usize>,
    pub line_count: Option<usize>,
    pub encoding: Option<String>,
}

impl ReadRequest {
    /// Validate the request ranges and string contract before filesystem I/O.
    ///
    /// # Errors
    ///
    /// Returns a validation error for empty/NUL path, zero line values, or a
    /// `line_count` above 2,000.
    pub fn validate(&self) -> Result<(), ReadError> {
        if self.path.is_empty() {
            return Err(ReadError::Validation("path must not be empty".to_owned()));
        }
        if self.path.contains('\0') {
            return Err(ReadError::Validation(
                "path must not contain NUL".to_owned(),
            ));
        }
        if self.start_line == Some(0) {
            return Err(ReadError::Validation(
                "start_line must be at least 1".to_owned(),
            ));
        }
        if let Some(line_count) = self.line_count
            && !(1..=MAX_LINE_COUNT).contains(&line_count)
        {
            return Err(ReadError::Validation(
                "line_count must be from 1 to 2000".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("invalid read request: {0}")]
    Validation(String),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("path cannot be represented losslessly in model-visible JSON")]
    NonUnicodePath,
    #[error("target is a directory; use glob to list its contents")]
    Directory,
    #[error("target is not a regular file")]
    NotRegular,
    #[error("cannot read binary, image, PDF, or executable content as source text")]
    Binary,
    #[error("file changed while it was being read")]
    Changed,
    #[error("read cancelled")]
    Cancelled,
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error(transparent)]
    Output(#[from] crate::output::OutputError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Read one regular source file through the retained repository capability.
///
/// # Errors
///
/// Returns a validation, path, I/O, encoding, consistency, cancellation, or
/// output-budget error without returning a mixed file version.
pub fn execute(
    root: &Arc<RepositoryRoot>,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<String, ReadError> {
    request.validate()?;
    let resolved = root.resolve(Path::new(&request.path))?;
    let absolute = resolved
        .absolute()
        .to_str()
        .ok_or(ReadError::NonUnicodePath)?
        .to_owned();
    match read_once(root, resolved.key(), &absolute, request, cancellation)? {
        Attempt::Stable(output) => return Ok(output),
        Attempt::Changed => {}
    }
    match read_once(root, resolved.key(), &absolute, request, cancellation) {
        Ok(Attempt::Stable(output)) => Ok(output),
        Ok(Attempt::Changed) => Err(ReadError::Changed),
        Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            Err(ReadError::Changed)
        }
        Err(error) => Err(error),
    }
}

enum Attempt<T> {
    Stable(T),
    Changed,
}

fn read_once(
    root: &RepositoryRoot,
    key: &Path,
    absolute: &str,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<Attempt<String>, ReadError> {
    if cancellation.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    let mut file = open_regular(root, key)?;
    let before = FileFingerprint::from_file(&file)?;
    run_before_read_hook();

    let mut prefix = Vec::with_capacity(PREFIX_BYTES);
    file.by_ref()
        .take(PREFIX_BYTES as u64)
        .read_to_end(&mut prefix)?;
    if has_binary_magic(&prefix) {
        return Err(ReadError::Binary);
    }
    let reader = io::Cursor::new(prefix).chain(&mut file);
    let mut collector = LineCollector::new(request.start_line.unwrap_or(1), request.line_count);
    let summary = decode_stream(
        reader,
        request.encoding.as_deref(),
        usize::MAX,
        cancellation,
        |chunk| Ok(collector.push(chunk)),
    )?;
    collector.finish_eof();
    run_after_read_hook();

    let after = FileFingerprint::from_file(&file)?;
    if before != after {
        return Ok(Attempt::Changed);
    }
    let identity = match open_regular(root, key) {
        Ok(identity) => FileFingerprint::from_file(&identity)?,
        Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Attempt::Changed);
        }
        Err(error) => return Err(error),
    };
    if before != identity {
        return Ok(Attempt::Changed);
    }

    render(
        absolute,
        request,
        summary.source_encoding,
        &collector,
        cancellation,
    )
    .map(Attempt::Stable)
}

fn open_regular(root: &RepositoryRoot, key: &Path) -> Result<File, ReadError> {
    let metadata = root.capability().symlink_metadata(key)?;
    if metadata.is_dir() {
        return Err(ReadError::Directory);
    }
    if !metadata.is_file() && !metadata.file_type().is_symlink() {
        return Err(ReadError::NotRegular);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = root.capability().open_with(key, &options)?;
    if !FileFingerprint::from_file(&file)?.regular {
        return Err(ReadError::NotRegular);
    }
    Ok(file)
}

fn has_binary_magic(prefix: &[u8]) -> bool {
    prefix.starts_with(b"%PDF-")
        || prefix.starts_with(b"\x7FELF")
        || prefix.starts_with(b"MZ")
        || prefix.starts_with(b"\x89PNG\r\n\x1A\n")
        || prefix.starts_with(&[0xFF, 0xD8, 0xFF])
        || prefix.starts_with(b"GIF87a")
        || prefix.starts_with(b"GIF89a")
        || prefix.starts_with(b"BM")
        || prefix.starts_with(b"II*\0")
        || prefix.starts_with(b"MM\0*")
        || prefix.starts_with(b"RIFF") && prefix.get(8..12) == Some(b"WEBP")
}

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
        for character in text.chars() {
            self.saw_input = true;
            if character == '\n' {
                self.ended_with_newline = true;
                if !self.finish_line() {
                    self.stopped = true;
                    return DecodeControl::Stop;
                }
            } else {
                self.ended_with_newline = false;
                self.current_bytes = self.current_bytes.saturating_add(character.len_utf8());
                if self.current.len() < LINE_PREFIX_BYTES {
                    self.current.push(character);
                }
            }
        }
        DecodeControl::Continue
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
) -> Result<String, ReadError> {
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
        let tail = if partial {
            continuation(
                absolute,
                collector.start.saturating_add(cap),
                request.encoding.as_deref(),
            )?
        } else {
            "Complete.".to_owned()
        };
        let mut formatter =
            OutputFormatter::new(header.clone(), vec![tail], OutputLimits::default())?;
        let mut shown = 0_usize;
        for line in collector.candidates.iter().take(cap) {
            let rendered = render_candidate(line);
            if formatter.try_push_line(rendered, cancellation)? {
                shown += 1;
                continue;
            }
            if shown == 0 {
                let minimal = format!("{}\t[line truncated: exceeds output budget]", line.number);
                if formatter.try_push_line(minimal, cancellation)? {
                    shown = 1;
                }
            }
            break;
        }
        if shown == cap {
            return formatter.finish(cancellation).map_err(ReadError::from);
        }
        cap = shown;
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

#[derive(Serialize)]
struct Continuation<'a> {
    path: &'a str,
    start_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding: Option<&'a str>,
}

fn continuation(
    path: &str,
    start_line: usize,
    encoding: Option<&str>,
) -> Result<String, ReadError> {
    let request = serde_json::to_string(&Continuation {
        path,
        start_line,
        encoding,
    })
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(format!("Partial: continue with {request}."))
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

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::{
        AFTER_READ_HOOK, BEFORE_READ_HOOK, MAX_LINE_COUNT, ReadError, ReadRequest, execute,
    };
    use crate::{output::token_count, path::RepositoryRoot};

    fn request(path: &str) -> ReadRequest {
        ReadRequest {
            path: path.to_owned(),
            start_line: None,
            line_count: None,
            encoding: None,
        }
    }

    #[test]
    fn reads_numbered_utf8_crlf_and_utf16_pages() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("utf8.txt"), "alpha\r\nbeta\n").expect("utf8");
        fs::write(fixture.path().join("utf8-bom.txt"), b"\xEF\xBB\xBFbom\n").expect("utf8 bom");
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "one\ntwo\nthree".encode_utf16() {
            utf16.extend(unit.to_le_bytes());
        }
        fs::write(fixture.path().join("utf16.txt"), utf16).expect("utf16");
        let mut utf16be = vec![0xFE, 0xFF];
        for unit in "big\nend".encode_utf16() {
            utf16be.extend(unit.to_be_bytes());
        }
        fs::write(fixture.path().join("utf16be.txt"), utf16be).expect("utf16be");
        fs::write(fixture.path().join("latin.txt"), [0x63, 0x61, 0x66, 0xE9])
            .expect("windows-1252");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let cancellation = CancellationToken::new();

        let utf8 = execute(&root, &request("utf8.txt"), &cancellation).expect("read utf8");
        assert!(utf8.contains("1\talpha\n2\tbeta\nComplete."));
        let bom = execute(&root, &request("utf8-bom.txt"), &cancellation).expect("utf8 bom");
        assert!(bom.contains("1\tbom"));

        let mut page = request("utf16.txt");
        page.start_line = Some(2);
        page.line_count = Some(1);
        let utf16 = execute(&root, &page, &cancellation).expect("read utf16");
        assert!(utf16.contains("Encoding: UTF-16LE\n2\ttwo"));
        assert!(utf16.ends_with("\"start_line\":3}."));
        let be = execute(&root, &request("utf16be.txt"), &cancellation).expect("utf16be");
        assert!(be.contains("Encoding: UTF-16BE\n1\tbig"));
        let mut latin = request("latin.txt");
        latin.encoding = Some("windows-1252".to_owned());
        let latin = execute(&root, &latin, &cancellation).expect("explicit encoding");
        assert!(latin.contains("Encoding: windows-1252\n1\tcafé"));
    }

    #[test]
    fn empty_long_binary_invalid_and_out_of_range_are_bounded() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("empty.txt"), "").expect("empty");
        fs::write(fixture.path().join("long.txt"), "x".repeat(100_000)).expect("long");
        fs::write(fixture.path().join("binary.bin"), b"\x89PNG\r\n\x1A\nrest").expect("binary");
        fs::write(fixture.path().join("invalid.txt"), [0xFF]).expect("invalid");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let cancellation = CancellationToken::new();

        let empty = execute(&root, &request("empty.txt"), &cancellation).expect("empty read");
        assert!(empty.ends_with("\nComplete."));
        let long = execute(&root, &request("long.txt"), &cancellation).expect("long read");
        assert!(long.contains("[line truncated]"));
        assert!(long.len() <= crate::output::MODEL_BYTE_LIMIT);
        assert!(token_count(&long) <= crate::output::MODEL_TOKEN_LIMIT);
        assert!(matches!(
            execute(&root, &request("binary.bin"), &cancellation),
            Err(ReadError::Binary)
        ));
        assert!(matches!(
            execute(&root, &request("invalid.txt"), &cancellation),
            Err(ReadError::Decode(_))
        ));

        let mut beyond = request("empty.txt");
        beyond.start_line = Some(100);
        let output = execute(&root, &beyond, &cancellation).expect("past eof");
        assert!(output.ends_with("\nComplete."));
    }

    #[test]
    fn retries_one_change_then_succeeds() {
        let fixture = tempfile::tempdir().expect("fixture");
        let path = fixture.path().join("race.txt");
        fs::write(&path, "old\n").expect("old");
        let changed = path.clone();
        BEFORE_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(&changed, "new\n").expect("change file");
            }));
        });
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let output = execute(&root, &request("race.txt"), &CancellationToken::new())
            .expect("retry succeeds");
        assert!(output.contains("1\tnew"));
    }

    #[test]
    fn second_change_fails_explicitly() {
        let fixture = tempfile::tempdir().expect("fixture");
        let path = fixture.path().join("race.txt");
        fs::write(&path, "old\n").expect("old");
        let changed = path.clone();
        AFTER_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(&changed, "first\n").expect("first change");
                let changed_again = changed.clone();
                BEFORE_READ_HOOK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        fs::write(&changed_again, "second\n").expect("second change");
                    }));
                });
            }));
        });
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        assert!(matches!(
            execute(&root, &request("race.txt"), &CancellationToken::new()),
            Err(ReadError::Changed)
        ));
    }

    #[test]
    fn replacement_and_deletion_never_return_mixed_content() {
        let fixture = tempfile::tempdir().expect("fixture");
        let path = fixture.path().join("replace.txt");
        fs::write(&path, "original\n").expect("original");
        let replace_path = path.clone();
        AFTER_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let old = replace_path.with_extension("old");
                fs::rename(&replace_path, old).expect("rename original");
                fs::write(&replace_path, "replacement\n").expect("replacement");
            }));
        });
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let output = execute(&root, &request("replace.txt"), &CancellationToken::new())
            .expect("replacement retry");
        assert!(output.contains("1\treplacement"));
        assert!(!output.contains("original"));

        let delete_path = path.clone();
        AFTER_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::remove_file(&delete_path).expect("delete file");
            }));
        });
        assert!(matches!(
            execute(&root, &request("replace.txt"), &CancellationToken::new()),
            Err(ReadError::Changed)
        ));
    }

    #[test]
    fn line_collector_uses_one_extra_line_probe() {
        let mut collector = super::LineCollector::new(1, Some(1));
        let control = collector.push("one\ntwo\nthree\nfour\n");
        assert_eq!(control, crate::encoding::DecodeControl::Stop);
        assert_eq!(collector.candidates.len(), 2);
        assert!(collector.stopped);
    }

    #[test]
    fn eof_at_candidate_budget_remains_partial() {
        let mut collector = super::LineCollector::new(1, None);
        let full_line = format!("{}\n", "x".repeat(super::LINE_PREFIX_BYTES));
        let mut text = full_line.repeat(super::CANDIDATE_BYTES / super::LINE_PREFIX_BYTES);
        text.push('y');

        assert_eq!(
            collector.push(&text),
            crate::encoding::DecodeControl::Continue
        );
        collector.finish_eof();

        assert!(collector.stopped);
    }

    #[test]
    fn webp_magic_requires_riff_container() {
        assert!(!super::has_binary_magic(b"abcdefghWEBP source text"));
        assert!(super::has_binary_magic(b"RIFF1234WEBP"));
    }

    #[test]
    fn validation_and_directory_fail_before_content_read() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir(fixture.path().join("directory")).expect("directory");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let cancellation = CancellationToken::new();

        let mut invalid = request("directory");
        invalid.line_count = Some(MAX_LINE_COUNT + 1);
        assert!(matches!(
            execute(&root, &invalid, &cancellation),
            Err(ReadError::Validation(_))
        ));
        assert!(matches!(
            execute(&root, &request("directory"), &cancellation),
            Err(ReadError::Directory)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn capability_allows_internal_symlink_and_blocks_escape() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::write(fixture.path().join("target.txt"), "inside\n").expect("target");
        fs::write(outside.path().join("secret.txt"), "outside\n").expect("secret");
        symlink("target.txt", fixture.path().join("inside-link")).expect("inside link");
        symlink(
            outside.path().join("secret.txt"),
            fixture.path().join("escape-link"),
        )
        .expect("escape link");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let cancellation = CancellationToken::new();

        let inside =
            execute(&root, &request("inside-link"), &cancellation).expect("internal symlink read");
        assert!(inside.contains("1\tinside"));
        assert!(execute(&root, &request("escape-link"), &cancellation).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn named_pipe_and_device_paths_are_rejected_without_blocking() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        let fixture = tempfile::tempdir().expect("fixture");
        let fifo = fixture.path().join("source.fifo");
        let fifo_bytes = CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path");
        assert_eq!(unsafe { libc::mkfifo(fifo_bytes.as_ptr(), 0o600) }, 0);
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let cancellation = CancellationToken::new();
        assert!(execute(&root, &request("source.fifo"), &cancellation).is_err());
        assert!(execute(&root, &request("/dev/null"), &cancellation).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_capability_allows_internal_symlink_and_blocks_reparse_escape() {
        use std::os::windows::fs::symlink_file;

        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::write(fixture.path().join("target.txt"), "inside\n").expect("target");
        fs::write(outside.path().join("secret.txt"), "outside\n").expect("secret");
        match symlink_file("target.txt", fixture.path().join("inside-link")) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) => {
                eprintln!("symbolic-link fixture unavailable: {error}");
                return;
            }
            Err(error) => panic!("inside link: {error}"),
        }
        match symlink_file(
            outside.path().join("secret.txt"),
            fixture.path().join("escape-link"),
        ) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) => {
                eprintln!("symbolic-link fixture unavailable: {error}");
                return;
            }
            Err(error) => panic!("escape reparse link: {error}"),
        }
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let cancellation = CancellationToken::new();

        let inside =
            execute(&root, &request("inside-link"), &cancellation).expect("internal link read");
        assert!(inside.contains("1\tinside"));
        assert!(execute(&root, &request("escape-link"), &cancellation).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_unicode_space_and_long_path_read() {
        let fixture = tempfile::tempdir().expect("fixture");
        let mut relative = std::path::PathBuf::from("Unicode space 界");
        for index in 0..12 {
            relative.push(format!("long-segment-{index:02}-xxxxxxxx"));
        }
        fs::create_dir_all(fixture.path().join(&relative)).expect("long path directories");
        relative.push("source file.rs");
        fs::write(fixture.path().join(&relative), "long path content\n").expect("long path file");
        assert!(fixture.path().join(&relative).as_os_str().len() > 260);
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let output = execute(
            &root,
            &request(&relative.to_string_lossy()),
            &CancellationToken::new(),
        )
        .expect("long path read");
        assert!(output.contains("1\tlong path content"));
    }
}
