use std::process::Command;

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
fn burst_budget_override_rejects_values_outside_the_safe_range() {
    for value in ["0", "2047", "32769", "many", "-1"] {
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
fn cursor_profile_uses_a_larger_default_burst_budget_than_codex() {
    let cursor = Command::new(env!("CARGO_BIN_EXE_codexshim"))
        .args(["doctor", "--client-profile", "cursor"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("CODEXSHIM_BURST_TOKENS")
        .env("CODEXSHIM_LOG_MODE", "off")
        .output()
        .expect("run cursor doctor");
    let codex = doctor(None);
    assert!(
        cursor.status.success(),
        "cursor doctor failed: {}",
        String::from_utf8_lossy(&cursor.stderr)
    );
    assert!(codex.status.success());

    let cursor_stdout = String::from_utf8(cursor.stdout).expect("cursor stdout");
    let codex_stdout = String::from_utf8(codex.stdout).expect("codex stdout");
    let burst = |stdout: &str| {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix("burst tokens:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .expect("doctor must report burst tokens")
    };
    assert!(cursor_stdout.contains("client profile: cursor"));
    assert!(burst(&cursor_stdout) > burst(&codex_stdout));
}

#[test]
fn invalid_search_memory_configuration_fails_before_runtime_startup() {
    let output = Command::new(env!("CARGO_BIN_EXE_codexshim"))
        .arg("doctor")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CODEXSHIM_GREP_MEMORY_BYTES", "many")
        .env("CODEXSHIM_LOG_MODE", "off")
        .output()
        .expect("run doctor");
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .expect("doctor stderr")
            .contains("CODEXSHIM_GREP_MEMORY_BYTES")
    );
}

#[test]
fn invalid_pdf_memory_configuration_fails_before_runtime_startup() {
    let output = Command::new(env!("CARGO_BIN_EXE_codexshim"))
        .arg("doctor")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CODEXSHIM_PDF_TEXT_MEMORY_BYTES", "1")
        .env("CODEXSHIM_LOG_MODE", "off")
        .output()
        .expect("run doctor");
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .expect("doctor stderr")
            .contains("CODEXSHIM_PDF_TEXT_MEMORY_BYTES")
    );
}

#[test]
fn invalid_process_capacity_fails_before_runtime_startup() {
    let output = doctor(Some("0"));
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .expect("doctor stderr")
            .contains("CODEXSHIM_PROCESS_CALLS")
    );
}
