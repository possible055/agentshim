use std::{
    collections::{BTreeMap, VecDeque},
    env, io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::Duration,
};

use chrono::Utc;
use serde_json::{Map, Value, json};
use tracing::{Event, Subscriber};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};
use uuid::Uuid;

use super::storage::{automatic_maintenance, prepare_directory, writer_loop};

pub const LOG_MODE_ENV: &str = "CODEXSHIM_LOG_MODE";
pub const LOG_DIR_ENV: &str = "CODEXSHIM_LOG_DIR";
const SCHEMA_VERSION: u64 = 1;
const FLIGHT_RECORDS: usize = 64;
const CHANNEL_BATCHES: usize = 1_024;
pub(super) const MAX_BATCH_RECORDS: usize = FLIGHT_RECORDS + 1;
const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;
pub(super) const WRITER_BATCH_WAIT: Duration = Duration::from_millis(10);
pub(super) const LOCK_WAIT: Duration = Duration::from_secs(1);
pub(super) const LOCK_RETRY: Duration = Duration::from_millis(5);
pub(super) const PART_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const DAY_BYTES: u64 = 128 * 1024 * 1024;
pub(super) const TOTAL_BYTES: u64 = 512 * 1024 * 1024;
pub(super) const RETENTION_DAYS: u64 = 30;
pub(super) const LINE_BYTES: usize = 8 * 1024;
const DIAGNOSTIC_BYTES: usize = 2 * 1024;

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
            | "shell_delegate"
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

pub(super) type Record = Map<String, Value>;
type Batch = Vec<Record>;

pub(super) struct QueuedBatch {
    pub(super) records: Batch,
    pub(super) charge: usize,
}

struct Recorder {
    mode: LogMode,
    instance_id: String,
    ring: Mutex<VecDeque<Record>>,
    sender: SyncSender<QueuedBatch>,
    writer: Option<Arc<LazyWriter>>,
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
        if let Some(writer) = &self.writer
            && let Err(error) = writer.start()
        {
            writer.warn_once(&error);
            return;
        }
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

enum WriterState {
    Pending(Receiver<QueuedBatch>),
    Running(thread::JoinHandle<()>),
    Disabled,
}

struct LazyWriter {
    directory: PathBuf,
    state: Mutex<WriterState>,
    started: AtomicBool,
    shutdown: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    queued_bytes: Arc<AtomicUsize>,
    warned: AtomicBool,
}

impl LazyWriter {
    fn new(
        directory: PathBuf,
        receiver: Receiver<QueuedBatch>,
        dropped: Arc<AtomicU64>,
        queued_bytes: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            directory,
            state: Mutex::new(WriterState::Pending(receiver)),
            started: AtomicBool::new(false),
            shutdown: Arc::new(AtomicBool::new(false)),
            dropped,
            queued_bytes,
            warned: AtomicBool::new(false),
        }
    }

    fn start(&self) -> io::Result<()> {
        if self.started.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.shutdown.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "diagnostics writer is shut down",
            ));
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            WriterState::Running(_) => {
                self.started.store(true, Ordering::Release);
                return Ok(());
            }
            WriterState::Disabled => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "diagnostics writer is disabled",
                ));
            }
            WriterState::Pending(_) => {}
        }
        if let Err(error) = prepare_directory(&self.directory) {
            *state = WriterState::Disabled;
            return Err(error);
        }
        let WriterState::Pending(receiver) = std::mem::replace(&mut *state, WriterState::Disabled)
        else {
            unreachable!("pending diagnostics writer state changed while locked");
        };
        let directory = self.directory.clone();
        let dropped = Arc::clone(&self.dropped);
        let queued_bytes = Arc::clone(&self.queued_bytes);
        let shutdown = Arc::clone(&self.shutdown);
        let writer = thread::Builder::new()
            .name("codexshim-log-writer".to_owned())
            .spawn(move || {
                let _ = automatic_maintenance(&directory);
                let mut warned = false;
                writer_loop(
                    &directory,
                    &receiver,
                    &dropped,
                    &queued_bytes,
                    &shutdown,
                    &mut warned,
                );
            })?;
        *state = WriterState::Running(writer);
        self.started.store(true, Ordering::Release);
        Ok(())
    }

    fn warn_once(&self, error: &io::Error) {
        if !self.warned.swap(true, Ordering::AcqRel) {
            eprintln!(
                "{}",
                crate::output::bounded_diagnostic(&format!(
                    "codexshim diagnostics writer disabled output: {error}"
                ))
            );
        }
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let writer = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match std::mem::replace(&mut *state, WriterState::Disabled) {
                WriterState::Running(writer) => Some(writer),
                WriterState::Pending(_) | WriterState::Disabled => None,
            }
        };
        if let Some(writer) = writer {
            let _ = writer.join();
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
    writer: Option<Arc<LazyWriter>>,
    pub mode: LogMode,
    pub directory: PathBuf,
    pub instance_id: String,
}

impl DiagnosticsGuard {
    #[must_use]
    pub fn disabled(directory: PathBuf) -> Self {
        Self {
            writer: None,
            mode: LogMode::Off,
            directory,
            instance_id: String::new(),
        }
    }

    /// Start a diagnostics recorder and return its tracing layer and shutdown guard.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when an eager diagnostics writer cannot be started.
    pub fn start(config: DiagnosticsConfig) -> io::Result<(Self, Option<DiagnosticsLayer>)> {
        if config.mode == LogMode::Off {
            return Ok((Self::disabled(config.directory), None));
        }
        let (sender, receiver) = mpsc::sync_channel::<QueuedBatch>(CHANNEL_BATCHES);
        let dropped = Arc::new(AtomicU64::new(0));
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let writer = Arc::new(LazyWriter::new(
            config.directory.clone(),
            receiver,
            Arc::clone(&dropped),
            Arc::clone(&queued_bytes),
        ));
        if config.mode == LogMode::All {
            writer.start()?;
        }
        let instance_id = Uuid::new_v4().to_string();
        let recorder = Arc::new(Recorder {
            mode: config.mode,
            instance_id: instance_id.clone(),
            ring: Mutex::new(VecDeque::with_capacity(FLIGHT_RECORDS)),
            sender,
            writer: Some(Arc::clone(&writer)),
            dropped,
            queued_bytes,
        });
        let layer = DiagnosticsLayer::new(recorder);
        Ok((
            Self {
                writer: Some(writer),
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
        if let Some(writer) = self.writer.take() {
            writer.shutdown();
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
