use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::{Days, NaiveDate, Utc};
use serde_json::{Value, json};

use super::core::{
    DAY_BYTES, DiagnosticsConfig, LINE_BYTES, LOCK_RETRY, LOCK_WAIT, LogMode, MAX_BATCH_RECORDS,
    PART_BYTES, QueuedBatch, RETENTION_DAYS, Record, TOTAL_BYTES, WRITER_BATCH_WAIT,
};

pub(super) fn writer_loop(
    directory: &Path,
    receiver: &Receiver<QueuedBatch>,
    dropped: &AtomicU64,
    queued_bytes: &AtomicUsize,
    shutdown: &AtomicBool,
    warned: &mut bool,
) {
    let mut pending = None;
    loop {
        let first = match pending.take() {
            Some(batch) => batch,
            None => loop {
                match receiver.recv_timeout(WRITER_BATCH_WAIT) {
                    Ok(batch) => break batch,
                    Err(RecvTimeoutError::Timeout) if shutdown.load(Ordering::Acquire) => return,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            },
        };
        queued_bytes.fetch_sub(first.charge, Ordering::AcqRel);
        let mut records = first.records;
        let deadline = Instant::now() + WRITER_BATCH_WAIT;
        while records.len() < MAX_BATCH_RECORDS {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match receiver.recv_timeout(remaining) {
                Ok(next) => {
                    queued_bytes.fetch_sub(next.charge, Ordering::AcqRel);
                    if records.len().saturating_add(next.records.len()) > MAX_BATCH_RECORDS {
                        pending = Some(next);
                        break;
                    }
                    records.extend(next.records);
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        if let Err(error) = write_batch(directory, &records) {
            dropped.fetch_add(
                u64::try_from(records.len()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            if !*warned {
                eprintln!(
                    "{}",
                    crate::output::bounded_diagnostic(&format!(
                        "codexshim diagnostics writer disabled output: {error}"
                    ))
                );
                *warned = true;
            }
        }
    }
}

pub(super) fn prepare_directory(directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(super) fn serialize_batch(batch: &[Record]) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    for record in batch {
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        if line.len() > LINE_BYTES {
            let mut record = record.clone();
            record.remove("diagnostic");
            record.insert(
                "diagnostic".to_owned(),
                json!("[diagnostic omitted: log line limit]"),
            );
            line = serde_json::to_vec(&record).map_err(io::Error::other)?;
        }
        if line.len() > LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "diagnostic log line exceeds 8192 bytes",
            ));
        }
        output.extend_from_slice(&line);
        output.push(b'\n');
    }
    Ok(output)
}

pub(super) fn write_batch(directory: &Path, batch: &[Record]) -> io::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let bytes = serialize_batch(batch)?;
    let date = Utc::now().date_naive();
    let lock_path = directory.join(format!("codexshim-{date}.lock"));
    let lock = open_private_append(&lock_path)?;
    acquire_lock(&lock, LOCK_WAIT)?;
    let result = append_rotated(directory, date, &bytes);
    let unlock = unlock_file(&lock);
    result.and(unlock)
}

pub(super) fn append_rotated(directory: &Path, date: NaiveDate, bytes: &[u8]) -> io::Result<()> {
    let incoming = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let mut daily = 0_u64;
    for part in 1..=2 {
        let path = log_path(directory, date, part);
        let length = fs::metadata(&path).map_or(0, |metadata| metadata.len());
        daily = daily.saturating_add(length);
        if length.saturating_add(incoming) <= PART_BYTES {
            if daily.saturating_add(incoming) > DAY_BYTES {
                break;
            }
            let mut file = open_private_append(&path)?;
            file.write_all(bytes)?;
            file.flush()?;
            return Ok(());
        }
    }
    Err(io::Error::other("daily diagnostic log limit reached"))
}

pub(super) fn log_path(directory: &Path, date: NaiveDate, part: u8) -> PathBuf {
    directory.join(format!("codexshim-{date}.{part:04}.jsonl"))
}

fn open_private_append(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn acquire_lock(file: &File, wait: Duration) -> io::Result<()> {
    let started = Instant::now();
    loop {
        if try_lock_file(file)? {
            return Ok(());
        }
        if started.elapsed() >= wait {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "diagnostic log lock timed out",
            ));
        }
        thread::sleep(LOCK_RETRY);
    }
}

#[cfg(unix)]
fn try_lock_file(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if matches!(error.kind(), io::ErrorKind::WouldBlock) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn unlock_file(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn try_lock_file(file: &File) -> io::Result<bool> {
    use std::{mem::zeroed, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::{ERROR_LOCK_VIOLATION, GetLastError},
        Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx},
        System::IO::OVERLAPPED,
    };
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &raw mut overlapped,
        )
    };
    if result != 0 {
        return Ok(true);
    }
    if unsafe { GetLastError() } == ERROR_LOCK_VIOLATION {
        Ok(false)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn unlock_file(file: &File) -> io::Result<()> {
    use std::{mem::zeroed, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{Storage::FileSystem::UnlockFileEx, System::IO::OVERLAPPED};
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    let result = unsafe {
        UnlockFileEx(
            file.as_raw_handle(),
            0,
            u32::MAX,
            u32::MAX,
            &raw mut overlapped,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[derive(Clone, Debug)]
pub(super) struct LogFile {
    pub(super) path: PathBuf,
    pub(super) date: NaiveDate,
    pub(super) bytes: u64,
}

pub(super) fn list_logs(directory: &Path) -> io::Result<Vec<LogFile>> {
    let mut logs = Vec::new();
    if !directory.exists() {
        return Ok(logs);
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(date) = parse_log_date(name) else {
            continue;
        };
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            logs.push(LogFile {
                path: entry.path(),
                date,
                bytes: metadata.len(),
            });
        }
    }
    logs.sort_by(|left, right| {
        left.date
            .cmp(&right.date)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(logs)
}

pub(super) fn parse_log_date(name: &str) -> Option<NaiveDate> {
    let remainder = name.strip_prefix("codexshim-")?;
    let (date, suffix) = remainder.split_at_checked(10)?;
    let part = suffix.strip_prefix('.')?.strip_suffix(".jsonl")?;
    if part.len() != 4 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

pub(super) fn automatic_maintenance(directory: &Path) -> io::Result<()> {
    let lock = open_private_append(&directory.join(".maintenance.lock"))?;
    if !try_lock_file(&lock)? {
        return Ok(());
    }
    let today = Utc::now().date_naive();
    let stamp_path = directory.join(".last-maintenance");
    let already_ran =
        fs::read_to_string(&stamp_path).is_ok_and(|value| value.trim() == today.to_string());
    let result = if already_ran {
        Ok(())
    } else {
        purge_directory(directory, today)
            .and_then(|_| write_private(&stamp_path, today.to_string().as_bytes()))
    };
    let unlock = unlock_file(&lock);
    result.and(unlock)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PurgeReport {
    pub files: usize,
    pub bytes: u64,
}

pub(super) fn purge_directory(directory: &Path, today: NaiveDate) -> io::Result<PurgeReport> {
    let cutoff = today
        .checked_sub_days(Days::new(RETENTION_DAYS))
        .unwrap_or(NaiveDate::MIN);
    let mut logs = list_logs(directory)?;
    let mut report = PurgeReport::default();
    for log in &logs {
        if log.date < cutoff && log.date != today {
            fs::remove_file(&log.path)?;
            report.files += 1;
            report.bytes = report.bytes.saturating_add(log.bytes);
        }
    }
    logs.retain(|log| log.path.exists());
    let mut total = logs.iter().map(|log| log.bytes).sum::<u64>();
    for log in logs {
        if total <= TOTAL_BYTES {
            break;
        }
        if log.date == today {
            continue;
        }
        fs::remove_file(&log.path)?;
        total = total.saturating_sub(log.bytes);
        report.files += 1;
        report.bytes = report.bytes.saturating_add(log.bytes);
    }
    Ok(report)
}

#[derive(Clone, Debug)]
pub struct LogStatus {
    pub mode: LogMode,
    pub directory: PathBuf,
    pub files: usize,
    pub bytes: u64,
    pub oldest: Option<NaiveDate>,
    pub newest: Option<NaiveDate>,
    pub dropped: u64,
}

/// Inspect the configured diagnostic log directory without modifying it.
///
/// # Errors
///
/// Returns configuration, directory, or JSON parsing errors encountered during inspection.
pub fn status(config: &DiagnosticsConfig) -> io::Result<LogStatus> {
    let logs = list_logs(&config.directory)?;
    let mut dropped = 0_u64;
    for log in &logs {
        let reader = BufReader::new(File::open(&log.path)?);
        for line in reader.lines() {
            let line = line?;
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                dropped = dropped.saturating_add(value["dropped_since_last"].as_u64().unwrap_or(0));
            }
        }
    }
    Ok(LogStatus {
        mode: config.mode,
        directory: config.directory.clone(),
        files: logs.len(),
        bytes: logs.iter().map(|log| log.bytes).sum(),
        oldest: logs.first().map(|log| log.date),
        newest: logs.last().map(|log| log.date),
        dropped,
    })
}

/// Apply retention and capacity limits while preserving today's active logs.
///
/// # Errors
///
/// Returns an I/O error when maintenance is busy or a selected old file cannot be removed.
pub fn purge(config: &DiagnosticsConfig) -> io::Result<PurgeReport> {
    prepare_directory(&config.directory)?;
    let lock = open_private_append(&config.directory.join(".maintenance.lock"))?;
    acquire_lock(&lock, LOCK_WAIT)?;
    let result = purge_directory(&config.directory, Utc::now().date_naive());
    let unlock = unlock_file(&lock);
    unlock?;
    result
}

#[must_use]
pub const fn retention_days() -> u64 {
    RETENTION_DAYS
}

#[must_use]
pub const fn capacity_bytes() -> u64 {
    TOTAL_BYTES
}
