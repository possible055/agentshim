use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::{Days, NaiveDate, Utc};
use serde_json::{Value, json};

use super::core::{
    DAY_BYTES, DiagnosticsConfig, EVENT_DAY_BYTES, HISTORICAL_BYTES, LINE_BYTES, LOCK_RETRY,
    LOCK_WAIT, LogMode, MAINTENANCE_RETRY, MAX_BATCH_RECORDS, PART_BYTES, QueueMetrics,
    QueuedBatch, RETENTION_DAYS, Record, TOTAL_BYTES, WRITER_BATCH_WAIT, base_record,
    batch_loss_count,
};

pub(super) fn writer_loop(
    directory: &Path,
    instance_id: &str,
    receiver: &Receiver<QueuedBatch>,
    dropped: &AtomicU64,
    queue: &QueueMetrics,
    shutdown: &AtomicBool,
    warned: &mut bool,
) {
    let mut pending = None;
    let mut maintenance = WriterMaintenance::default();
    'writer: loop {
        let first = match pending.take() {
            Some(batch) => batch,
            None => loop {
                match receiver.recv_timeout(WRITER_BATCH_WAIT) {
                    Ok(batch) => break batch,
                    Err(RecvTimeoutError::Timeout) if shutdown.load(Ordering::Acquire) => {
                        break 'writer;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break 'writer,
                }
            },
        };
        queue.batches.fetch_sub(1, Ordering::AcqRel);
        queue.bytes.fetch_sub(first.charge, Ordering::AcqRel);
        #[cfg(test)]
        queue.run_writer_hook();
        let mut records = first.records;
        let deadline = Instant::now() + WRITER_BATCH_WAIT;
        while records.len() < MAX_BATCH_RECORDS {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match receiver.recv_timeout(remaining) {
                Ok(next) => {
                    if records.len().saturating_add(next.records.len()) > MAX_BATCH_RECORDS {
                        pending = Some(next);
                        break;
                    }
                    queue.batches.fetch_sub(1, Ordering::AcqRel);
                    queue.bytes.fetch_sub(next.charge, Ordering::AcqRel);
                    records.extend(next.records);
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        let date = Utc::now().date_naive();
        match maintenance.prepare(directory, date, Instant::now()) {
            Ok(true) => records.insert(0, maintenance_recovered_record(instance_id)),
            Ok(false) => {}
            Err(_) => {
                restore_batch_loss(dropped, &records);
                if maintenance.take_warning() {
                    eprintln!("agentshim diagnostics maintenance unavailable; retrying");
                }
                continue;
            }
        }
        if let Err(error) = write_batch_at(directory, &records, date) {
            restore_batch_loss(dropped, &records);
            if !*warned {
                eprintln!(
                    "{}",
                    crate::output::bounded_diagnostic(&format!(
                        "agentshim diagnostics writer disabled output: {error}"
                    ))
                );
                *warned = true;
            }
        }
    }
    write_shutdown_summary(directory, instance_id, dropped, maintenance.failed);
}

#[derive(Default)]
pub(super) struct WriterMaintenance {
    maintained_date: Option<NaiveDate>,
    retry_at: Option<Instant>,
    failed: bool,
    warning_pending: bool,
}

impl WriterMaintenance {
    pub(super) fn prepare(
        &mut self,
        directory: &Path,
        date: NaiveDate,
        now: Instant,
    ) -> io::Result<bool> {
        if self.maintained_date == Some(date) {
            return Ok(false);
        }
        if self.retry_at.is_some_and(|retry_at| retry_at > now) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "diagnostics maintenance retry pending",
            ));
        }
        match automatic_maintenance(directory, date) {
            Ok(()) => {
                self.maintained_date = Some(date);
                self.retry_at = None;
                let recovered = std::mem::take(&mut self.failed);
                Ok(recovered)
            }
            Err(error) => {
                self.retry_at = Some(now + MAINTENANCE_RETRY);
                if !self.failed {
                    self.warning_pending = true;
                }
                self.failed = true;
                Err(error)
            }
        }
    }

    fn take_warning(&mut self) -> bool {
        std::mem::take(&mut self.warning_pending)
    }
}

pub(super) fn restore_batch_loss(dropped: &AtomicU64, records: &[Record]) {
    dropped.fetch_add(batch_loss_count(records), Ordering::Relaxed);
}

fn maintenance_recovered_record(instance_id: &str) -> Record {
    let mut record = base_record(instance_id, "INFO", "diagnostics_maintenance");
    record.insert("phase".to_owned(), json!("storage"));
    record.insert("outcome".to_owned(), json!("success"));
    record.insert("reason".to_owned(), json!("retry_recovered"));
    record
}

pub(super) fn write_shutdown_summary(
    directory: &Path,
    instance_id: &str,
    dropped: &AtomicU64,
    maintenance_failed: bool,
) {
    let count = dropped.swap(0, Ordering::AcqRel);
    if count == 0 {
        return;
    }
    let date = Utc::now().date_naive();
    if automatic_maintenance(directory, date).is_err() {
        dropped.fetch_add(count, Ordering::Relaxed);
        eprintln!("agentshim diagnostics shutdown summary unavailable");
        return;
    }
    let mut records = Vec::with_capacity(2);
    if maintenance_failed {
        records.push(maintenance_recovered_record(instance_id));
    }
    let mut summary = base_record(instance_id, "WARN", "diagnostics_drop_summary");
    summary.insert("phase".to_owned(), json!("shutdown"));
    summary.insert("reason".to_owned(), json!("shutdown"));
    summary.insert("dropped_since_last".to_owned(), json!(count));
    records.push(summary);
    if write_summary_at(directory, &records, date).is_err() {
        dropped.fetch_add(count, Ordering::Relaxed);
        eprintln!("agentshim diagnostics shutdown summary unavailable");
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

#[cfg(test)]
pub(super) fn write_batch(directory: &Path, batch: &[Record]) -> io::Result<()> {
    write_batch_at(directory, batch, Utc::now().date_naive())
}

pub(super) fn write_batch_at(
    directory: &Path,
    batch: &[Record],
    date: NaiveDate,
) -> io::Result<()> {
    write_batch_with_limit(directory, batch, date, EVENT_DAY_BYTES)
}

fn write_summary_at(directory: &Path, batch: &[Record], date: NaiveDate) -> io::Result<()> {
    write_batch_with_limit(directory, batch, date, DAY_BYTES)
}

fn write_batch_with_limit(
    directory: &Path,
    batch: &[Record],
    date: NaiveDate,
    daily_limit: u64,
) -> io::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let bytes = serialize_batch(batch)?;
    let lock_path = directory.join(format!("agentshim-{date}.lock"));
    let lock = open_private_append(&lock_path)?;
    acquire_lock(&lock, LOCK_WAIT)?;
    let result = append_rotated_with_limit(directory, date, &bytes, daily_limit);
    let unlock = crate::platform::diagnostics::unlock_file(&lock);
    result.and(unlock)
}

#[cfg(test)]
pub(super) fn append_rotated(directory: &Path, date: NaiveDate, bytes: &[u8]) -> io::Result<()> {
    append_rotated_with_limit(directory, date, bytes, EVENT_DAY_BYTES)
}

fn append_rotated_with_limit(
    directory: &Path,
    date: NaiveDate,
    bytes: &[u8],
    daily_limit: u64,
) -> io::Result<()> {
    let incoming = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let mut daily = 0_u64;
    for part in 1..=2 {
        let path = log_path(directory, date, part);
        let length = fs::metadata(&path).map_or(0, |metadata| metadata.len());
        daily = daily.saturating_add(length);
        if length.saturating_add(incoming) <= PART_BYTES {
            if daily.saturating_add(incoming) > daily_limit {
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
    directory.join(format!("agentshim-{date}.{part:04}.jsonl"))
}

fn open_private_append(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)?;
    crate::platform::diagnostics::set_private_permissions(&file)?;
    Ok(file)
}

fn acquire_lock(file: &File, wait: Duration) -> io::Result<()> {
    let started = Instant::now();
    loop {
        if crate::platform::diagnostics::try_lock_file(file)? {
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
    let remainder = name.strip_prefix("agentshim-")?;
    let (date, suffix) = remainder.split_at_checked(10)?;
    let part = suffix.strip_prefix('.')?.strip_suffix(".jsonl")?;
    if part.len() != 4 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

pub(super) fn automatic_maintenance(directory: &Path, today: NaiveDate) -> io::Result<()> {
    let lock = open_private_append(&directory.join(".maintenance.lock"))?;
    acquire_lock(&lock, LOCK_WAIT)?;
    let stamp_path = directory.join(".last-maintenance");
    let stamp = format!("strict-{today}");
    let already_ran = fs::read_to_string(&stamp_path).is_ok_and(|value| value.trim() == stamp);
    let result = if already_ran {
        Ok(())
    } else {
        purge_directory(directory, today).and_then(|_| write_private(&stamp_path, stamp.as_bytes()))
    };
    let unlock = crate::platform::diagnostics::unlock_file(&lock);
    result.and(unlock)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    crate::platform::diagnostics::set_private_permissions(&file)?;
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
    let mut historical = logs
        .iter()
        .filter(|log| log.date != today)
        .map(|log| log.bytes)
        .sum::<u64>();
    for log in logs {
        if historical <= HISTORICAL_BYTES {
            break;
        }
        if log.date == today {
            continue;
        }
        fs::remove_file(&log.path)?;
        historical = historical.saturating_sub(log.bytes);
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
    let unlock = crate::platform::diagnostics::unlock_file(&lock);
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
