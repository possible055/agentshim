use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use agentshim_core::tools::exec::spawn::CaptureSink;

pub const MIN_CAPTURE_MAX_BYTES: u64 = 1024 * 1024;
pub const MAX_CAPTURE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_CAPTURE_MAX_BYTES: u64 = 64 * 1024 * 1024;

pub const CAPTURE_LIMIT_EXCEEDED_CODE: &str = "AGENTSHIM_CAPTURE_LIMIT_EXCEEDED";
pub const CAPTURE_IO_FAILED_CODE: &str = "AGENTSHIM_CAPTURE_IO_FAILED";

/// One published capture artifact stream.
#[derive(Clone, Debug)]
pub(crate) struct ArtifactRecord {
    pub path: PathBuf,
    pub bytes: u64,
    pub complete: bool,
    pub stream: String,
    pub valid_text: bool,
}

/// Per-call durable capture: one unguessable session directory, one exclusive
/// file per stream, aggregate byte ceiling, fsync before publication.
pub(crate) struct CallCapture {
    directory: PathBuf,
    streams: Vec<(String, Mutex<File>)>,
    totals: Vec<AtomicU64>,
    written: AtomicU64,
    max_bytes: u64,
    exceeded: AtomicBool,
    io_failed: Mutex<Option<String>>,
}

impl CallCapture {
    pub fn create(
        root: &Path,
        session_key: &str,
        call_key: &str,
        streams: &[&str],
        max_bytes: u64,
    ) -> io::Result<Self> {
        let directory = root.join(session_key).join(call_key);
        std::fs::create_dir_all(root.join(session_key))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                root.join(session_key),
                std::fs::Permissions::from_mode(0o700),
            );
        }
        std::fs::create_dir(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700));
        }
        let mut opened = Vec::new();
        for stream in streams {
            let path = directory.join(format!("{stream}.bin"));
            let file = OpenOptions::new()
                .append(true)
                .create_new(true)
                .open(&path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            opened.push(((*stream).to_owned(), Mutex::new(file)));
        }
        Ok(Self {
            directory,
            totals: streams.iter().map(|_| AtomicU64::new(0)).collect(),
            streams: opened,
            written: AtomicU64::new(0),
            max_bytes,
            exceeded: AtomicBool::new(false),
            io_failed: Mutex::new(None),
        })
    }

    fn stream_index(&self, stream: usize) -> usize {
        stream.min(self.streams.len().saturating_sub(1))
    }

    pub fn exceeded(&self) -> bool {
        self.exceeded.load(Ordering::SeqCst)
    }

    pub fn io_failure(&self) -> Option<String> {
        self.io_failed.lock().ok().and_then(|guard| guard.clone())
    }

    fn note_io_failure(&self, message: String) {
        if let Ok(mut guard) = self.io_failed.lock() {
            if guard.is_none() {
                *guard = Some(message);
            }
        }
    }

    /// Flush and fsync every stream, then publish the artifact records. Called
    /// once the process tree has fully settled.
    pub fn publish(&self, complete: bool) -> io::Result<Vec<ArtifactRecord>> {
        let mut records = Vec::new();
        for (index, entry) in self.streams.iter().enumerate() {
            let bytes = self.totals[index].load(Ordering::SeqCst);
            let Ok(mut guard) = entry.1.lock() else {
                return Err(io::Error::other("capture stream lock poisoned"));
            };
            guard.flush()?;
            guard.sync_all()?;
            let path = self.directory.join(format!("{}.bin", entry.0));
            let head = Self::text_probe(&path, bytes)?;
            records.push(ArtifactRecord {
                path,
                bytes,
                complete,
                stream: entry.0.clone(),
                valid_text: head,
            });
        }
        Ok(records)
    }

    /// True when the whole capture is valid UTF-8 without NUL bytes.
    fn text_probe(path: &Path, bytes: u64) -> io::Result<bool> {
        let limit = u64::from(u32::MAX);
        if bytes > limit {
            return Ok(false);
        }
        let data = std::fs::read(path)?;
        Ok(!data.contains(&0) && std::str::from_utf8(&data).is_ok())
    }

    /// Remove the session directory when nothing about the call needs an artifact.
    pub fn discard(&self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl CaptureSink for CallCapture {
    fn append(&self, stream: usize, bytes: &[u8]) -> io::Result<()> {
        if self.exceeded.load(Ordering::SeqCst) {
            return Err(io::Error::other(CAPTURE_LIMIT_EXCEEDED_CODE));
        }
        let index = self.stream_index(stream);
        let next = self
            .written
            .fetch_add(bytes.len() as u64, Ordering::SeqCst)
            .saturating_add(bytes.len() as u64);
        if next > self.max_bytes {
            self.exceeded.store(true, Ordering::SeqCst);
            return Err(io::Error::other(CAPTURE_LIMIT_EXCEEDED_CODE));
        }
        let result = (|| {
            let Ok(mut guard) = self.streams[index].1.lock() else {
                return Err(io::Error::other("capture stream lock poisoned"));
            };
            guard.write_all(bytes)
        })();
        if let Err(error) = result {
            self.note_io_failure(error.to_string());
            return Err(error);
        }
        self.totals[index].fetch_add(bytes.len() as u64, Ordering::SeqCst);
        Ok(())
    }

    fn complete(&self, _complete: bool, _error: Option<&str>) -> io::Result<()> {
        Ok(())
    }
}

/// Decide whether a completed capture needs a durable artifact at all.
pub(crate) fn should_publish(records: &[ArtifactRecord], complete: bool) -> bool {
    if !complete {
        return true;
    }
    let total: u64 = records.iter().map(|record| record.bytes).sum();
    if total == 0 {
        return false;
    }
    if total > crate::budget::DEFAULT_PAGE_BUDGET_BYTES as u64 {
        return true;
    }
    records
        .iter()
        .any(|record| record.bytes > 0 && !record.valid_text)
}
