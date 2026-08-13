use std::{fs, process::Command};

fn doctor(process_calls: Option<&str>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codexshim"));
    command
        .arg("doctor")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("CODEXSHIM_PROCESS_CALLS")
        .env_remove("CODEXSHIM_DETACHED_CALLS")
        .env_remove("CODEXSHIM_GREP_MEMORY_BYTES")
        .env_remove("CODEXSHIM_GLOB_MEMORY_BYTES")
        .env_remove("CODEXSHIM_BURST_TOKENS")
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
        assert!(stdout.contains("grep memory bytes: 268435456"));
        assert!(stdout.contains("glob memory bytes: 33554432"));
        assert!(stdout.contains("global memory bytes: 268435456"));
        assert!(stdout.contains("burst tokens: 8192"));
        assert!(stdout.contains(&format!("blocking threads: {blocking_threads}")));
    }
}

#[test]
fn burst_budget_can_only_be_lowered_inside_its_safe_range() {
    for (value, expected) in [("2048", "2048"), ("4096", "4096"), ("8192", "8192")] {
        let output = Command::new(env!("CARGO_BIN_EXE_codexshim"))
            .arg("doctor")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("CODEXSHIM_BURST_TOKENS", value)
            .env("CODEXSHIM_LOG_MODE", "off")
            .output()
            .expect("run doctor");
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("doctor stdout");
        assert!(stdout.contains(&format!("burst tokens: {expected}")));
    }
    for value in ["0", "2047", "8193", "many", "-1"] {
        let output = Command::new(env!("CARGO_BIN_EXE_codexshim"))
            .arg("doctor")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("CODEXSHIM_BURST_TOKENS", value)
            .env("CODEXSHIM_LOG_MODE", "off")
            .output()
            .expect("run doctor");
        assert!(!output.status.success(), "{value} must be rejected");
        assert!(
            String::from_utf8(output.stderr)
                .expect("doctor stderr")
                .contains("CODEXSHIM_BURST_TOKENS")
        );
    }
}

#[test]
fn invalid_search_memory_configuration_fails_before_runtime_startup() {
    for variable in ["CODEXSHIM_GREP_MEMORY_BYTES", "CODEXSHIM_GLOB_MEMORY_BYTES"] {
        for value in ["8388607", "1073741825", "-1", "many"] {
            let output = Command::new(env!("CARGO_BIN_EXE_codexshim"))
                .arg("doctor")
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .env_remove("CODEXSHIM_GREP_MEMORY_BYTES")
                .env_remove("CODEXSHIM_GLOB_MEMORY_BYTES")
                .env(variable, value)
                .env("CODEXSHIM_LOG_MODE", "off")
                .output()
                .expect("run doctor");
            assert!(!output.status.success(), "{variable}={value} must fail");
            let stderr = String::from_utf8(output.stderr).expect("doctor stderr");
            assert!(stderr.contains(variable), "{stderr}");
        }
    }
}

#[test]
fn doctor_reports_overridden_search_and_global_memory_capacity() {
    let output = Command::new(env!("CARGO_BIN_EXE_codexshim"))
        .arg("doctor")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CODEXSHIM_GREP_MEMORY_BYTES", "1073741824")
        .env("CODEXSHIM_GLOB_MEMORY_BYTES", "8388608")
        .env("CODEXSHIM_LOG_MODE", "off")
        .output()
        .expect("run doctor");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("doctor stdout");
    assert!(stdout.contains("grep memory bytes: 1073741824"));
    assert!(stdout.contains("glob memory bytes: 8388608"));
    assert!(stdout.contains("global memory bytes: 1073741824"));
}

#[test]
fn doctor_reports_pdf_mode_reservations_and_whether_they_are_charged() {
    let output = Command::new(env!("CARGO_BIN_EXE_codexshim"))
        .arg("doctor")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CODEXSHIM_PDF_TEXT_MEMORY_BYTES", "33554432")
        .env("CODEXSHIM_PDF_IMAGE_MEMORY_BYTES", "201326592")
        .env("CODEXSHIM_LOG_MODE", "off")
        .output()
        .expect("run doctor");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("doctor stdout");
    assert!(stdout.contains("pdf text memory bytes: 33554432"));
    assert!(stdout.contains("pdf image memory bytes: 201326592"));

    // The page ceilings are derived from those reservations, so reporting them proves
    // end to end that configuring a reservation configures what the parser enforces —
    // not just what the scheduler bills.
    let spans = |label: &str| -> usize {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(label))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_else(|| panic!("doctor must report {label}, got:\n{stdout}"))
    };
    let text_spans = spans("pdf text page spans:");
    let image_spans = spans("pdf image page spans:");
    assert!(text_spans > 0, "a page must always be allowed some text");
    assert!(
        text_spans < image_spans,
        "the smaller reservation must yield the smaller page ceiling, got {text_spans} and {image_spans}"
    );
}

/// Each PDF variable has its own range; a value legal for grep or glob is not
/// automatically legal here.
#[test]
fn invalid_pdf_memory_configuration_fails_before_runtime_startup() {
    for (variable, value, bound) in [
        ("CODEXSHIM_PDF_TEXT_MEMORY_BYTES", "33554431", "33554432"),
        ("CODEXSHIM_PDF_TEXT_MEMORY_BYTES", "134217729", "134217728"),
        ("CODEXSHIM_PDF_TEXT_MEMORY_BYTES", "1073741824", "134217728"),
        ("CODEXSHIM_PDF_IMAGE_MEMORY_BYTES", "67108863", "67108864"),
        ("CODEXSHIM_PDF_IMAGE_MEMORY_BYTES", "201326593", "201326592"),
        ("CODEXSHIM_PDF_IMAGE_MEMORY_BYTES", "8388608", "67108864"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_codexshim"))
            .arg("doctor")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env(variable, value)
            .env("CODEXSHIM_LOG_MODE", "off")
            .output()
            .expect("run doctor");
        assert!(
            !output.status.success(),
            "{variable}={value} must be rejected"
        );
        let stderr = String::from_utf8(output.stderr).expect("doctor stderr");
        assert!(
            stderr.contains(variable) && stderr.contains(bound),
            "unexpected error for {variable}={value}: {stderr}"
        );
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
        .env_remove("CODEXSHIM_GREP_MEMORY_BYTES")
        .env_remove("CODEXSHIM_GLOB_MEMORY_BYTES")
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
    assert!(contents.contains(",grep_memory_bytes=268435456"));
    assert!(contents.contains(",glob_memory_bytes=33554432"));
    assert!(contents.contains(",memory_bytes=268435456"));
}
