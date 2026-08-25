use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
};

use chrono::{Days, NaiveDate};
use tracing_subscriber::prelude::*;

use super::*;
use crate::diagnostics::storage::{
    WriterMaintenance, append_rotated, list_logs, log_path, parse_log_date, purge_directory,
    serialize_batch, status, write_batch, write_shutdown_summary, writer_loop,
};

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
fn blocked_writer_join_has_a_deadline() {
    let directory = tempfile::tempdir().expect("diagnostic directory");
    let (sender, receiver) = mpsc::sync_channel(1);
    let queue = Arc::new(QueueMetrics::new());
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let writer_entered = Arc::clone(&entered);
    let writer_release = Arc::clone(&release);
    queue.set_writer_hook(Arc::new(move || {
        writer_entered.wait();
        writer_release.wait();
    }));
    let writer = LazyWriter::new(
        directory.path().to_path_buf(),
        "blocked-writer".to_owned(),
        receiver,
        Arc::new(AtomicU64::new(0)),
        Arc::clone(&queue),
    );
    writer.start().expect("start real writer");
    queue.batches.fetch_add(1, Ordering::AcqRel);
    queue.bytes.fetch_add(LINE_BYTES, Ordering::AcqRel);
    sender
        .send(QueuedBatch {
            records: vec![record("blocked_writer")],
            charge: LINE_BYTES,
        })
        .expect("queue batch");
    entered.wait();

    let started = std::time::Instant::now();
    assert!(!writer.shutdown_with_timeout(Duration::from_millis(20)));
    assert!(started.elapsed() < Duration::from_millis(100));
    assert_eq!(queue.batches.load(Ordering::Acquire), 0);
    assert_eq!(queue.bytes.load(Ordering::Acquire), 0);

    release.wait();
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while list_logs(directory.path()).expect("list logs").is_empty()
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!list_logs(directory.path()).expect("list logs").is_empty());
}

#[test]
fn completed_writer_is_joined() {
    let writer = std::thread::spawn(|| {});
    assert!(join_writer_until(writer, Duration::from_secs(1)));
}

#[test]
fn pending_writer_batch_keeps_queue_metrics_balanced() {
    let directory = tempfile::tempdir().expect("diagnostic directory");
    let (sender, receiver) = mpsc::sync_channel(2);
    let queue = QueueMetrics::new();
    let first_records = vec![record("first"); MAX_BATCH_RECORDS - 1];
    let second_records = vec![record("second"); 2];
    let first_charge = first_records.len() * LINE_BYTES;
    let second_charge = second_records.len() * LINE_BYTES;
    queue.batches.store(2, Ordering::Release);
    queue
        .bytes
        .store(first_charge + second_charge, Ordering::Release);
    sender
        .send(QueuedBatch {
            records: first_records,
            charge: first_charge,
        })
        .expect("queue first batch");
    sender
        .send(QueuedBatch {
            records: second_records,
            charge: second_charge,
        })
        .expect("queue second batch");
    drop(sender);

    writer_loop(
        directory.path(),
        "pending-batch",
        &receiver,
        &AtomicU64::new(0),
        &queue,
        &AtomicBool::new(false),
        &mut false,
    );

    assert_eq!(queue.batches.load(Ordering::Acquire), 0);
    assert_eq!(queue.bytes.load(Ordering::Acquire), 0);
}

#[test]
fn configuration_resolves_defaults_and_rejects_invalid_values() {
    let absolute = std::env::current_dir().expect("absolute directory");
    let config = DiagnosticsConfig::from_values(None, None, || Ok(absolute.clone()))
        .expect("default config");
    assert_eq!(config.mode, LogMode::Errors);
    assert_eq!(config.directory, absolute);

    assert!(
        DiagnosticsConfig::from_values(Some("verbose".into()), None, || Ok(std::env::current_dir(
        )
        .expect("directory")),)
        .is_err()
    );
    assert!(
        DiagnosticsConfig::from_values(Some("all".into()), Some("relative/logs".into()), || Ok(
            std::env::current_dir().expect("directory")
        ),)
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
    assert!(allowed_field("shell_delegate"));
    for field in [
        "tool_count",
        "toolset",
        "has_cursor",
        "cache_ttl_ms",
        "cache_scope",
        "client_profile",
        "tool_output_tokens",
        "burst_tokens",
        "frame_limit_bytes",
        "framework",
        "framework_target",
        "framework_event",
        "request_id",
    ] {
        assert!(
            allowed_field(field),
            "control-plane field rejected: {field}"
        );
    }
}

#[test]
fn control_plane_fields_are_persisted_without_sensitive_inputs() {
    let (recorder, receiver) = test_recorder(LogMode::All, 1);
    let subscriber = tracing_subscriber::registry().with(DiagnosticsLayer::new(Arc::new(recorder)));

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(
            target: "agentshim",
            event = "tools_list",
            tool_count = 6_u64,
            toolset = "read,grep,glob,run_program,bash,bash_status",
            has_cursor = false,
            cache_ttl_ms = 300_000_u64,
            cache_scope = "private",
            client_profile = "cursor",
            tool_output_tokens = 8_192_u64,
            burst_tokens = 32_768_u64,
            frame_limit_bytes = 8_388_608_u64,
            framework = "rmcp",
            framework_target = "rmcp::service",
            framework_event = "request_rejected",
            request_id = "31d54ba8-c1c4-4efd-b596-47208981144a",
            arguments = "sensitive input"
        );
    });

    let record = receiver
        .recv()
        .expect("control-plane record")
        .records
        .remove(0);
    assert_eq!(record["tool_count"], 6);
    assert_eq!(
        record["toolset"],
        "read,grep,glob,run_program,bash,bash_status"
    );
    assert_eq!(record["has_cursor"], false);
    assert_eq!(record["cache_ttl_ms"], 300_000);
    assert_eq!(record["cache_scope"], "private");
    assert_eq!(record["client_profile"], "cursor");
    assert_eq!(record["tool_output_tokens"], 8_192);
    assert_eq!(record["burst_tokens"], 32_768);
    assert_eq!(record["frame_limit_bytes"], 8_388_608);
    assert_eq!(record["framework"], "rmcp");
    assert_eq!(record["framework_target"], "rmcp::service");
    assert_eq!(record["framework_event"], "request_rejected");
    assert_eq!(record["request_id"], "31d54ba8-c1c4-4efd-b596-47208981144a");
    assert!(record.get("arguments").is_none());
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
        parse_log_date("agentshim-2026-08-06.0001.jsonl"),
        NaiveDate::from_ymd_opt(2026, 8, 6)
    );
    assert_eq!(parse_log_date("agentshim-2026-08-06.lock"), None);
    assert_eq!(parse_log_date("other-2026-08-06.0001.jsonl"), None);
    assert_eq!(parse_log_date("agentshim-2026-08-06.bad.jsonl"), None);
    assert_eq!(
        parse_log_date("agentshim-2026-08-06.0001.extra.jsonl"),
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
    let queue = Arc::new(QueueMetrics::new());
    (
        Recorder {
            mode,
            instance_id: "test-instance".to_owned(),
            ring: Mutex::new(VecDeque::with_capacity(FLIGHT_RECORDS)),
            sender,
            writer: None,
            dropped: Arc::new(AtomicU64::new(0)),
            queue,
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
fn repeated_queue_overflow_preserves_prior_drop_debt() {
    let (recorder, receiver) = test_recorder(LogMode::All, 1);
    recorder.record("INFO", fields("queued"));
    recorder.record("INFO", fields("first-drop"));
    assert_eq!(recorder.dropped.load(Ordering::Acquire), 1);
    recorder.record("INFO", fields("second-drop"));
    assert_eq!(recorder.dropped.load(Ordering::Acquire), 2);

    receiver.recv().expect("queued batch");
    recorder.record("INFO", fields("recovery"));
    let recovery = receiver.recv().expect("recovery batch").records;
    assert_eq!(recovery[0]["dropped_since_last"], 2);
}

#[test]
fn errors_mode_starts_writer_only_when_an_error_is_recorded() {
    let parent = tempfile::tempdir().expect("parent");
    let directory = parent.path().join("logs");
    let (guard, layer) = DiagnosticsGuard::start(DiagnosticsConfig {
        mode: LogMode::Errors,
        directory: directory.clone(),
    })
    .expect("lazy diagnostics");
    let layer = layer.expect("diagnostics layer");

    assert!(!directory.exists());
    layer.recorder.record("INFO", fields("context"));
    assert!(!directory.exists());
    layer.recorder.record("ERROR", fields("trigger"));
    drop(guard);

    let logs = list_logs(&directory).expect("logs");
    assert_eq!(logs.len(), 1);
    let lines = BufReader::new(File::open(&logs[0].path).expect("log"))
        .lines()
        .collect::<io::Result<Vec<_>>>()
        .expect("lines");
    assert_eq!(lines.len(), 2);
}

#[test]
fn all_mode_starts_writer_eagerly() {
    let parent = tempfile::tempdir().expect("parent");
    let directory = parent.path().join("logs");
    let (guard, _) = DiagnosticsGuard::start(DiagnosticsConfig {
        mode: LogMode::All,
        directory: directory.clone(),
    })
    .expect("eager diagnostics");
    assert!(directory.exists());
    drop(guard);
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

#[test]
fn capacity_purge_reserves_a_full_active_day() {
    let directory = tempfile::tempdir().expect("directory");
    let today = NaiveDate::from_ymd_opt(2026, 8, 6).expect("date");
    for (days, part) in [(2, 1), (1, 1)] {
        let date = today
            .checked_sub_days(Days::new(days))
            .expect("historical date");
        File::create(log_path(directory.path(), date, part))
            .expect("historical log")
            .set_len(200 * 1024 * 1024)
            .expect("historical size");
    }

    purge_directory(directory.path(), today).expect("purge");
    let historical = list_logs(directory.path())
        .expect("logs")
        .iter()
        .filter(|log| log.date != today)
        .map(|log| log.bytes)
        .sum::<u64>();
    assert!(historical <= HISTORICAL_BYTES);
}

#[test]
fn ordinary_events_leave_space_for_the_shutdown_summary() {
    let directory = tempfile::tempdir().expect("directory");
    let today = chrono::Utc::now().date_naive();
    File::create(log_path(directory.path(), today, 1))
        .expect("part one")
        .set_len(PART_BYTES)
        .expect("part one size");
    let second_size = EVENT_DAY_BYTES - PART_BYTES;
    File::create(log_path(directory.path(), today, 2))
        .expect("part two")
        .set_len(second_size)
        .expect("part two size");
    assert!(append_rotated(directory.path(), today, b"x\n").is_err());

    let dropped = AtomicU64::new(3);
    write_shutdown_summary(directory.path(), "test-instance", &dropped, false);
    assert_eq!(dropped.load(Ordering::Acquire), 0);
    assert!(
        fs::metadata(log_path(directory.path(), today, 2))
            .expect("part two metadata")
            .len()
            > second_size
    );
}

#[test]
fn shutdown_summary_is_counted_and_zero_drops_write_nothing() {
    let directory = tempfile::tempdir().expect("directory");
    let zero = AtomicU64::new(0);
    write_shutdown_summary(directory.path(), "test-instance", &zero, false);
    assert!(list_logs(directory.path()).expect("zero logs").is_empty());

    let dropped = AtomicU64::new(4);
    write_shutdown_summary(directory.path(), "test-instance", &dropped, false);
    let status = status(&DiagnosticsConfig {
        mode: LogMode::All,
        directory: directory.path().to_owned(),
    })
    .expect("status");
    assert_eq!(status.dropped, 4);
    let contents = fs::read_to_string(&list_logs(directory.path()).expect("logs")[0].path)
        .expect("summary log");
    let summary: Value = serde_json::from_str(contents.trim()).expect("summary JSON");
    assert_eq!(summary["event"], "diagnostics_drop_summary");
    assert_eq!(summary["reason"], "shutdown");
}

#[test]
fn writer_maintenance_tracks_utc_dates_and_retries_failures() {
    let parent = tempfile::tempdir().expect("parent");
    let directory = parent.path().join("logs");
    fs::create_dir(&directory).expect("log directory");
    let first = NaiveDate::from_ymd_opt(2026, 8, 6).expect("first date");
    let second = first.checked_add_days(Days::new(1)).expect("second date");
    let started = std::time::Instant::now();
    let mut maintenance = WriterMaintenance::default();

    assert!(
        !maintenance
            .prepare(&directory, first, started)
            .expect("first maintenance")
    );
    assert_eq!(
        fs::read_to_string(directory.join(".last-maintenance")).expect("first stamp"),
        "strict-2026-08-06"
    );
    assert!(
        !maintenance
            .prepare(&directory, second, started)
            .expect("second maintenance")
    );
    assert_eq!(
        fs::read_to_string(directory.join(".last-maintenance")).expect("second stamp"),
        "strict-2026-08-07"
    );

    let blocked = parent.path().join("blocked");
    fs::write(&blocked, b"file").expect("blocking file");
    let mut retrying = WriterMaintenance::default();
    assert!(retrying.prepare(&blocked, first, started).is_err());
    fs::remove_file(&blocked).expect("remove blocking file");
    fs::create_dir(&blocked).expect("replacement directory");
    assert!(
        retrying
            .prepare(&blocked, first, started + Duration::from_secs(59))
            .is_err()
    );
    assert!(
        retrying
            .prepare(&blocked, first, started + Duration::from_secs(61))
            .expect("retried maintenance")
    );
}
