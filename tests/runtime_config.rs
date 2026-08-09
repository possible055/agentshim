use std::{fs, process::Command};

fn doctor(process_calls: Option<&str>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codexshim"));
    command
        .arg("doctor")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("CODEXSHIM_PROCESS_CALLS")
        .env_remove("CODEXSHIM_DETACHED_CALLS")
        .env("CODEXSHIM_LOG_MODE", "off");
    if let Some(process_calls) = process_calls {
        command.env("CODEXSHIM_PROCESS_CALLS", process_calls);
    }
    command.output().expect("run doctor")
}

#[test]
fn doctor_reports_resolved_runtime_capacity() {
    for (configured, process_calls, blocking_threads) in
        [(None, 16, 50), (Some("1"), 1, 35), (Some("32"), 32, 66)]
    {
        let output = doctor(configured);
        assert!(
            output.status.success(),
            "doctor failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("doctor stdout");
        assert!(stdout.contains("read-only calls: 16"));
        assert!(stdout.contains(&format!("process calls: {process_calls}")));
        assert!(stdout.contains("detached calls: 16"));
        assert!(stdout.contains(&format!("blocking threads: {blocking_threads}")));
    }
}

#[test]
fn invalid_process_capacity_fails_before_runtime_startup() {
    for value in ["0", "33", "-1", "many"] {
        let output = doctor(Some(value));
        assert!(!output.status.success(), "{value} must be rejected");
        let stderr = String::from_utf8(output.stderr).expect("doctor stderr");
        assert!(
            stderr.contains("CODEXSHIM_PROCESS_CALLS must be an integer from 1 to 32"),
            "unexpected error for {value}: {stderr}"
        );
    }
}

#[test]
fn startup_log_records_resolved_runtime_capacity() {
    let logs = tempfile::tempdir().expect("log directory");
    let output = Command::new(env!("CARGO_BIN_EXE_codexshim"))
        .arg("doctor")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CODEXSHIM_PROCESS_CALLS", "32")
        .env("CODEXSHIM_DETACHED_CALLS", "16")
        .env("CODEXSHIM_LOG_MODE", "all")
        .env("CODEXSHIM_LOG_DIR", logs.path())
        .output()
        .expect("run doctor with diagnostics");
    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_dir(logs.path())
        .expect("read log directory")
        .map(|entry| entry.expect("log entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .expect("JSONL log");
    let contents = fs::read_to_string(log).expect("read startup log");
    assert!(
        contents.contains("process_calls=32,detached_calls=16,read_only_calls=16,worker_lanes=")
    );
    assert!(contents.contains(",blocking_threads=66"));
}
