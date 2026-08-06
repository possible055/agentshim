use std::{
    collections::{BTreeMap, VecDeque},
    env, fs,
    fs::{File, OpenOptions},
    io,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::{Days, NaiveDate, Utc};
use serde_json::{Map, Value, json};
use tracing::{Event, Subscriber};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};
use uuid::Uuid;

pub const LOG_MODE_ENV: &str = "CODEXSHIM_LOG_MODE";
pub const LOG_DIR_ENV: &str = "CODEXSHIM_LOG_DIR";
const SCHEMA_VERSION: u64 = 1;
const FLIGHT_RECORDS: usize = 64;
const CHANNEL_BATCHES: usize = 1_024;
const MAX_BATCH_RECORDS: usize = FLIGHT_RECORDS + 1;
const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;
const WRITER_BATCH_WAIT: Duration = Duration::from_millis(10);
const LOCK_WAIT: Duration = Duration::from_secs(1);
const LOCK_RETRY: Duration = Duration::from_millis(5);
const PART_BYTES: u64 = 64 * 1024 * 1024;
const DAY_BYTES: u64 = 128 * 1024 * 1024;
const TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const RETENTION_DAYS: u64 = 30;
const LINE_BYTES: usize = 8 * 1024;
const DIAGNOSTIC_BYTES: usize = 2 * 1024;

#[derive(Debug)]
pub(crate) struct DetailedExecution {
    pub output: String,
    pub run_ms: u64,
}

impl DetailedExecution {
    pub(crate) fn measure<E>(operation: impl FnOnce() -> Result<String, E>) -> Result<Self, E> {
        let started = Instant::now();
        operation().map(|output| Self {
            output,
            run_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogMode {
    Off,
    #[default]
    Errors,
    All,
}

impl std::fmt::Display for LogMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::Errors => "errors",
            Self::All => "all",
        })
    }
}

impl std::str::FromStr for LogMode {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "errors" => Ok(Self::Errors),
            "all" => Ok(Self::All),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{LOG_MODE_ENV} must be one of `off`, `errors`, or `all`"),
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiagnosticsConfig {
    pub mode: LogMode,
    pub directory: PathBuf,
}

impl DiagnosticsConfig {
    /// Resolve the diagnostics mode and storage directory from the process environment.
    ///
    /// # Errors
    ///
    /// Returns invalid input for malformed settings or when no platform state directory exists.
    pub fn from_env() -> io::Result<Self> {
        Self::from_values(
            env::var_os(LOG_MODE_ENV),
            env::var_os(LOG_DIR_ENV),
            default_log_directory,
        )
    }

    fn from_values(
        mode: Option<std::ffi::OsString>,
        directory: Option<std::ffi::OsString>,
        default_directory: impl FnOnce() -> io::Result<PathBuf>,
    ) -> io::Result<Self> {
        let mode = match mode {
            None => LogMode::default(),
            Some(value) => value
                .into_string()
                .map_err(|_| invalid_env(LOG_MODE_ENV, "must be valid Unicode"))?
                .parse()?,
        };
        let directory = match directory {
            Some(value) => {
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    return Err(invalid_env(LOG_DIR_ENV, "must be an absolute path"));
                }
                path
            }
            None => default_directory()?,
        };
        Ok(Self { mode, directory })
    }
}

fn invalid_env(name: &str, reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{name} {reason}"))
}

#[cfg(windows)]
fn default_log_directory() -> io::Result<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("codexshim").join("logs"))
        .ok_or_else(|| invalid_env("LOCALAPPDATA", "must contain an absolute path"))
}

#[cfg(not(windows))]
fn default_log_directory() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        if !path.is_absolute() {
            return Err(invalid_env("XDG_STATE_HOME", "must be an absolute path"));
        }
        return Ok(path.join("codexshim").join("logs"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join(".local/state/codexshim/logs"))
        .ok_or_else(|| invalid_env("HOME", "must contain an absolute path"))
}

#[derive(Clone)]
struct SpanFields(BTreeMap<String, Value>);

#[derive(Default)]
struct FieldVisitor {
    values: BTreeMap<String, Value>,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.insert(field.name(), json!(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.insert(field.name(), json!(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.insert(field.name(), json!(value));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        let limit = if field.name() == "diagnostic" {
            DIAGNOSTIC_BYTES
        } else {
            1024
        };
        self.insert(field.name(), json!(truncate_utf8(value, limit)));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }
}

impl FieldVisitor {
    fn insert(&mut self, name: &str, value: Value) {
        if allowed_field(name) {
            self.values.insert(name.to_owned(), value);
        }
    }
}

fn allowed_field(name: &str) -> bool {
    matches!(
        name,
        "event"
            | "call_id"
            | "tool"
            | "phase"
            | "outcome"
            | "queue_ms"
            | "run_ms"
            | "error_class"
            | "io_kind"
            | "os_code"
            | "counters"
            | "context"
            | "dropped_since_last"
            | "root"
            | "protocol"
            | "client_name"
            | "client_version"
            | "read_scope"
            | "reason"
            | "diagnostic"
    )
}

fn truncate_utf8(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit.saturating_sub(3).min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

type Record = Map<String, Value>;
type Batch = Vec<Record>;

struct QueuedBatch {
    records: Batch,
    charge: usize,
}

struct Recorder {
    mode: LogMode,
    instance_id: String,
    ring: Mutex<VecDeque<Record>>,
    sender: SyncSender<QueuedBatch>,
    dropped: Arc<AtomicU64>,
    queued_bytes: Arc<AtomicUsize>,
}

impl Recorder {
    fn record(&self, level: &str, mut fields: BTreeMap<String, Value>) {
        let event = fields
            .remove("event")
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "tracing_event".to_owned());
        let mut record = Map::new();
        record.insert("schema_version".to_owned(), json!(SCHEMA_VERSION));
        record.insert("ts".to_owned(), json!(Utc::now().to_rfc3339()));
        record.insert("level".to_owned(), json!(level));
        record.insert("event".to_owned(), json!(event));
        record.insert("instance_id".to_owned(), json!(self.instance_id));
        record.insert("pid".to_owned(), json!(std::process::id()));
        record.insert("version".to_owned(), json!(env!("CARGO_PKG_VERSION")));
        for (key, value) in fields {
            record.insert(key, value);
        }
        match self.mode {
            LogMode::Off => {}
            LogMode::All => self.send(vec![record]),
            LogMode::Errors if matches!(level, "WARN" | "ERROR") => {
                let mut ring = self
                    .ring
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut batch = ring.drain(..).collect::<Vec<_>>();
                for context in &mut batch {
                    context.insert("context".to_owned(), json!(true));
                }
                batch.push(record);
                drop(ring);
                self.send(batch);
            }
            LogMode::Errors => {
                let mut ring = self
                    .ring
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if ring.len() == FLIGHT_RECORDS {
                    ring.pop_front();
                }
                ring.push_back(record);
            }
        }
    }

    fn send(&self, mut batch: Batch) {
        let dropped = self.dropped.swap(0, Ordering::AcqRel);
        if dropped > 0 {
            if let Some(record) = batch.last_mut() {
                record.insert("dropped_since_last".to_owned(), json!(dropped));
            }
        }
        while batch.len() > MAX_BATCH_RECORDS {
            let remaining = batch.split_off(MAX_BATCH_RECORDS);
            self.send_one(batch);
            batch = remaining;
        }
        self.send_one(batch);
    }

    fn send_one(&self, batch: Batch) {
        let batch_len = u64::try_from(batch.len()).unwrap_or(u64::MAX);
        let charge = batch.len().saturating_mul(LINE_BYTES);
        if self
            .queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued
                    .checked_add(charge)
                    .filter(|total| *total <= MAX_QUEUED_BYTES)
            })
            .is_err()
        {
            self.dropped.fetch_add(batch_len, Ordering::Relaxed);
            return;
        }
        match self.sender.try_send(QueuedBatch {
            records: batch,
            charge,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.queued_bytes.fetch_sub(charge, Ordering::AcqRel);
                self.dropped.fetch_add(batch_len, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Clone)]
pub struct DiagnosticsLayer {
    recorder: Arc<Recorder>,
}

impl DiagnosticsLayer {
    fn new(recorder: Arc<Recorder>) -> Self {
        Self { recorder }
    }
}

impl<S> Layer<S> for DiagnosticsLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        context: Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor::default();
        attributes.record(&mut visitor);
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(SpanFields(visitor.values));
        }
    }

    fn on_record(
        &self,
        id: &tracing::Id,
        values: &tracing::span::Record<'_>,
        context: Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor::default();
        values.record(&mut visitor);
        if let Some(span) = context.span(id) {
            let mut extensions = span.extensions_mut();
            if let Some(fields) = extensions.get_mut::<SpanFields>() {
                fields.0.extend(visitor.values);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let metadata = event.metadata();
        if metadata.target() != "codexshim" && !metadata.target().starts_with("codexshim::") {
            if metadata.target().starts_with("rmcp")
                && matches!(
                    *metadata.level(),
                    tracing::Level::WARN | tracing::Level::ERROR
                )
            {
                self.recorder.record(
                    metadata.level().as_str(),
                    BTreeMap::from([("event".to_owned(), json!("rmcp_internal"))]),
                );
            }
            return;
        }
        let mut fields = BTreeMap::new();
        if let Some(scope) = context.event_scope(event) {
            for span in scope.from_root() {
                if let Some(span_fields) = span.extensions().get::<SpanFields>() {
                    fields.extend(
                        span_fields
                            .0
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone())),
                    );
                }
            }
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        fields.extend(visitor.values);
        self.recorder.record(metadata.level().as_str(), fields);
    }
}

pub struct DiagnosticsGuard {
    sender: Option<SyncSender<QueuedBatch>>,
    writer: Option<thread::JoinHandle<()>>,
    shutdown: Option<Arc<AtomicBool>>,
    pub mode: LogMode,
    pub directory: PathBuf,
    pub instance_id: String,
}

impl DiagnosticsGuard {
    #[must_use]
    pub fn disabled(directory: PathBuf) -> Self {
        Self {
            sender: None,
            writer: None,
            shutdown: None,
            mode: LogMode::Off,
            directory,
            instance_id: String::new(),
        }
    }

    /// Start a diagnostics recorder and return its tracing layer and shutdown guard.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the configured directory cannot be prepared.
    pub fn start(config: DiagnosticsConfig) -> io::Result<(Self, Option<DiagnosticsLayer>)> {
        if config.mode == LogMode::Off {
            return Ok((Self::disabled(config.directory), None));
        }
        prepare_directory(&config.directory)?;
        let (sender, receiver) = mpsc::sync_channel::<QueuedBatch>(CHANNEL_BATCHES);
        let directory = config.directory.clone();
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_dropped = Arc::clone(&dropped);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let writer_queued_bytes = Arc::clone(&queued_bytes);
        let shutdown = Arc::new(AtomicBool::new(false));
        let writer_shutdown = Arc::clone(&shutdown);
        let writer = thread::Builder::new()
            .name("codexshim-log-writer".to_owned())
            .spawn(move || {
                let _ = automatic_maintenance(&directory);
                let mut warned = false;
                writer_loop(
                    &directory,
                    &receiver,
                    &writer_dropped,
                    &writer_queued_bytes,
                    &writer_shutdown,
                    &mut warned,
                );
            })?;
        let instance_id = Uuid::new_v4().to_string();
        let recorder = Arc::new(Recorder {
            mode: config.mode,
            instance_id: instance_id.clone(),
            ring: Mutex::new(VecDeque::with_capacity(FLIGHT_RECORDS)),
            sender: sender.clone(),
            dropped,
            queued_bytes,
        });
        let layer = DiagnosticsLayer::new(recorder);
        Ok((
            Self {
                sender: Some(sender),
                writer: Some(writer),
                shutdown: Some(shutdown),
                mode: config.mode,
                directory: config.directory,
                instance_id,
            },
            Some(layer),
        ))
    }
}

impl Drop for DiagnosticsGuard {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.store(true, Ordering::Release);
        }
        self.sender.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn writer_loop(
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

fn prepare_directory(directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn serialize_batch(batch: &[Record]) -> io::Result<Vec<u8>> {
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

fn write_batch(directory: &Path, batch: &[Record]) -> io::Result<()> {
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

fn append_rotated(directory: &Path, date: NaiveDate, bytes: &[u8]) -> io::Result<()> {
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

fn log_path(directory: &Path, date: NaiveDate, part: u8) -> PathBuf {
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
struct LogFile {
    path: PathBuf,
    date: NaiveDate,
    bytes: u64,
}

fn list_logs(directory: &Path) -> io::Result<Vec<LogFile>> {
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

fn parse_log_date(name: &str) -> Option<NaiveDate> {
    let remainder = name.strip_prefix("codexshim-")?;
    let (date, suffix) = remainder.split_at_checked(10)?;
    let part = suffix.strip_prefix('.')?.strip_suffix(".jsonl")?;
    if part.len() != 4 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

fn automatic_maintenance(directory: &Path) -> io::Result<()> {
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

fn purge_directory(directory: &Path, today: NaiveDate) -> io::Result<PurgeReport> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record(event: &str) -> Record {
        Map::from_iter([
            ("schema_version".to_owned(), json!(1)),
            ("ts".to_owned(), json!("2026-08-06T00:00:00Z")),
            ("level".to_owned(), json!("ERROR")),
            ("event".to_owned(), json!(event)),
            ("instance_id".to_owned(), json!("instance")),
            ("pid".to_owned(), json!(1)),
            ("version".to_owned(), json!("test")),
        ])
    }

    #[test]
    fn mode_parser_is_strict() {
        assert_eq!("off".parse::<LogMode>().expect("mode"), LogMode::Off);
        assert_eq!("errors".parse::<LogMode>().expect("mode"), LogMode::Errors);
        assert_eq!("all".parse::<LogMode>().expect("mode"), LogMode::All);
        assert!("debug".parse::<LogMode>().is_err());
    }

    #[test]
    fn configuration_resolves_defaults_and_rejects_invalid_values() {
        let absolute = std::env::current_dir().expect("absolute directory");
        let config = DiagnosticsConfig::from_values(None, None, || Ok(absolute.clone()))
            .expect("default config");
        assert_eq!(config.mode, LogMode::Errors);
        assert_eq!(config.directory, absolute);

        assert!(
            DiagnosticsConfig::from_values(Some("verbose".into()), None, || Ok(
                std::env::current_dir().expect("directory")
            ),)
            .is_err()
        );
        assert!(
            DiagnosticsConfig::from_values(
                Some("all".into()),
                Some("relative/logs".into()),
                || Ok(std::env::current_dir().expect("directory")),
            )
            .is_err()
        );
    }

    #[test]
    fn field_allowlist_redacts_sensitive_inputs_and_outputs() {
        for field in [
            "arguments",
            "pattern",
            "stdin",
            "argv",
            "environment",
            "source",
            "stdout",
            "stderr",
        ] {
            assert!(!allowed_field(field), "sensitive field admitted: {field}");
        }
        assert!(allowed_field("call_id"));
        assert!(allowed_field("error_class"));
    }

    #[test]
    fn batch_is_json_lines_and_bounded() {
        let mut value = record("failed");
        value.insert("diagnostic".to_owned(), json!("界".repeat(LINE_BYTES)));
        let bytes = serialize_batch(&[value]).expect("serialize");
        let line = std::str::from_utf8(&bytes).expect("UTF-8").trim();
        let parsed: Value = serde_json::from_str(line).expect("JSON");
        assert_eq!(parsed["schema_version"], 1);
        assert!(line.len() <= LINE_BYTES);
    }

    #[test]
    fn log_name_parser_rejects_non_logs() {
        assert_eq!(
            parse_log_date("codexshim-2026-08-06.0001.jsonl"),
            NaiveDate::from_ymd_opt(2026, 8, 6)
        );
        assert_eq!(parse_log_date("codexshim-2026-08-06.lock"), None);
        assert_eq!(parse_log_date("other-2026-08-06.0001.jsonl"), None);
        assert_eq!(parse_log_date("codexshim-2026-08-06.bad.jsonl"), None);
        assert_eq!(
            parse_log_date("codexshim-2026-08-06.0001.extra.jsonl"),
            None
        );
    }

    #[test]
    fn purge_keeps_today_and_removes_expired_logs() {
        let directory = tempfile::tempdir().expect("directory");
        let today = NaiveDate::from_ymd_opt(2026, 8, 6).expect("date");
        fs::write(log_path(directory.path(), today, 1), b"today").expect("today");
        let expired = today.checked_sub_days(Days::new(31)).expect("expired");
        fs::write(log_path(directory.path(), expired, 1), b"expired").expect("expired");
        let report = purge_directory(directory.path(), today).expect("purge");
        assert_eq!(report.files, 1);
        assert!(log_path(directory.path(), today, 1).exists());
        assert!(!log_path(directory.path(), expired, 1).exists());
    }

    fn test_recorder(mode: LogMode, capacity: usize) -> (Recorder, mpsc::Receiver<QueuedBatch>) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        (
            Recorder {
                mode,
                instance_id: "test-instance".to_owned(),
                ring: Mutex::new(VecDeque::with_capacity(FLIGHT_RECORDS)),
                sender,
                dropped: Arc::new(AtomicU64::new(0)),
                queued_bytes,
            },
            receiver,
        )
    }

    fn fields(event: &str) -> BTreeMap<String, Value> {
        BTreeMap::from([("event".to_owned(), json!(event))])
    }

    #[test]
    fn errors_mode_drains_only_the_last_64_context_events() {
        let (recorder, receiver) = test_recorder(LogMode::Errors, 1);
        for index in 0..70 {
            recorder.record("INFO", fields(&format!("context-{index}")));
        }
        recorder.record("ERROR", fields("trigger"));
        let batch = receiver.recv().expect("batch").records;
        assert_eq!(batch.len(), FLIGHT_RECORDS + 1);
        assert_eq!(batch[0]["event"], "context-6");
        assert_eq!(batch[0]["context"], true);
        assert_eq!(batch.last().expect("trigger")["event"], "trigger");
        assert!(batch.last().expect("trigger").get("context").is_none());
    }

    #[test]
    fn all_off_and_overflow_modes_are_non_blocking_and_report_drops() {
        let (off, off_receiver) = test_recorder(LogMode::Off, 1);
        off.record("ERROR", fields("ignored"));
        assert!(off_receiver.try_recv().is_err());

        let (all, receiver) = test_recorder(LogMode::All, 1);
        all.record("INFO", fields("first"));
        all.record("INFO", fields("dropped"));
        assert_eq!(receiver.recv().expect("first").records[0]["event"], "first");
        all.record("INFO", fields("summary"));
        let summary = receiver.recv().expect("summary").records;
        assert_eq!(summary[0]["event"], "summary");
        assert_eq!(summary[0]["dropped_since_last"], 1);
    }

    #[test]
    fn concurrent_batches_are_complete_json_lines() {
        let directory = tempfile::tempdir().expect("directory");
        let directory = Arc::new(directory.path().to_owned());
        let mut writers = Vec::new();
        for writer in 0..4 {
            let directory = Arc::clone(&directory);
            writers.push(thread::spawn(move || {
                let mut written = 0;
                for sequence in 0..20 {
                    match write_batch(
                        &directory,
                        &[record(&format!("writer-{writer}-{sequence}"))],
                    ) {
                        Ok(()) => written += 1,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(error) => panic!("append: {error}"),
                    }
                }
                written
            }));
        }
        let mut written = 0;
        for writer in writers {
            written += writer.join().expect("writer");
        }
        let logs = list_logs(&directory).expect("logs");
        assert_eq!(logs.len(), 1);
        let reader = BufReader::new(File::open(&logs[0].path).expect("log"));
        let lines = reader
            .lines()
            .collect::<io::Result<Vec<_>>>()
            .expect("lines");
        assert_eq!(lines.len(), written);
        assert!(written > 0);
        assert!(
            lines
                .iter()
                .all(|line| serde_json::from_str::<Value>(line).is_ok())
        );
    }

    #[test]
    fn rotation_and_capacity_purge_preserve_today() {
        let directory = tempfile::tempdir().expect("directory");
        let today = NaiveDate::from_ymd_opt(2026, 8, 6).expect("date");
        File::create(log_path(directory.path(), today, 1))
            .expect("part one")
            .set_len(PART_BYTES)
            .expect("size");
        append_rotated(directory.path(), today, b"line\n").expect("rotate");
        assert_eq!(
            fs::read(log_path(directory.path(), today, 2)).expect("part two"),
            b"line\n"
        );

        let old = today.checked_sub_days(Days::new(1)).expect("old");
        for part in 1..=2 {
            File::create(log_path(directory.path(), old, part))
                .expect("old part")
                .set_len(300 * 1024 * 1024)
                .expect("old size");
        }
        let report = purge_directory(directory.path(), today).expect("purge");
        assert!(report.files >= 1);
        assert!(log_path(directory.path(), today, 1).exists());
        assert!(log_path(directory.path(), today, 2).exists());
    }
}
