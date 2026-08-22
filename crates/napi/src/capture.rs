use std::{
    fs::{File, OpenOptions},
    io::{self, Read as _, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use agentshim_core::tools::exec::CaptureSink;

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
    pub(crate) max_bytes: u64,
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

    /// Resolve a capture-stream id to its slot index. Unknown ids are rejected
    /// rather than clamped so output never lands in a neighbouring stream's
    /// file.
    fn stream_index(&self, stream: usize) -> io::Result<usize> {
        if stream >= self.streams.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown capture stream {stream}; this call captures {} streams",
                    self.streams.len()
                ),
            ));
        }
        Ok(stream)
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

    /// True when the whole capture is valid UTF-8 without NUL bytes. Streams
    /// the file in bounded chunks so a multi-gigabyte artifact is never held
    /// in memory; a UTF-8 character split across a chunk boundary is carried
    /// into the next round.
    fn text_probe(path: &Path, bytes: u64) -> io::Result<bool> {
        const CHUNK: usize = 64 * 1024;
        const CEILING: u64 = u32::MAX as u64;
        if bytes > CEILING {
            return Ok(false);
        }
        let mut file = File::open(path)?;
        let mut carry: Vec<u8> = Vec::new();
        let mut chunk = vec![0_u8; CHUNK];
        loop {
            let read = file.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            carry.extend_from_slice(&chunk[..read]);
            match std::str::from_utf8(&carry) {
                Ok(text) => {
                    if text.contains('\0') {
                        return Ok(false);
                    }
                    carry.clear();
                }
                Err(error) if error.error_len().is_none() => {
                    if carry[..error.valid_up_to()].contains(&0) {
                        return Ok(false);
                    }
                    carry.drain(..error.valid_up_to());
                }
                Err(_) => return Ok(false),
            }
        }
        Ok(std::str::from_utf8(&carry).is_ok_and(|text| !text.contains('\0')))
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
        let index = self.stream_index(stream)?;
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
pub(crate) fn should_publish(
    records: &[ArtifactRecord],
    complete: bool,
    inline_output_bytes: u64,
) -> bool {
    if !complete {
        return true;
    }
    let total: u64 = records.iter().map(|record| record.bytes).sum();
    if total == 0 {
        return false;
    }
    if total > inline_output_bytes {
        return true;
    }
    records
        .iter()
        .any(|record| record.bytes > 0 && !record.valid_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agentshim-capture-test-{}-{tag}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn unknown_stream_ids_are_rejected_not_clamped() {
        let root = capture_root("streams");
        let capture = CallCapture::create(&root, "session", "call", &["output"], 4096).unwrap();
        let error = capture
            .append(3, b"x")
            .expect_err("an out-of-range stream id must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        capture.append(0, b"hello").unwrap();
        let published = capture.publish(true).unwrap();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].stream, "output");
        assert_eq!(published[0].bytes, 5);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn text_probe_streams_boundaries_and_refuses_invalid_text() {
        let root = capture_root("probe");
        let good = CallCapture::create(&root, "session", "good", &["output"], 1 << 20).unwrap();
        let mut payload = vec![b'a'; 64 * 1024 - 1];
        payload.extend_from_slice("€".as_bytes());
        payload.extend_from_slice(&vec![b'b'; 128 * 1024]);
        good.append(0, &payload).unwrap();
        let published = good.publish(true).unwrap();
        assert!(published[0].valid_text);

        let bad = CallCapture::create(&root, "session", "bad", &["output"], 1 << 20).unwrap();
        bad.append(0, &[0xFF, 0xFE]).unwrap();
        assert!(!bad.publish(true).unwrap()[0].valid_text);

        let nul = CallCapture::create(&root, "session", "nul", &["output"], 1 << 20).unwrap();
        nul.append(0, b"a\0b").unwrap();
        assert!(!nul.publish(true).unwrap()[0].valid_text);
        std::fs::remove_dir_all(&root).ok();
    }
}
