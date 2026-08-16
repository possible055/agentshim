use super::common::{fixtures::*, session::*};
use super::*;

fn doctor(profile: &str, timeout: Option<&str>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentshim"));
    command
        .args(["doctor", "--client-profile", profile])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("AGENTSHIM_IDLE_TIMEOUT")
        .env("AGENTSHIM_LOG_MODE", "off");
    if let Some(timeout) = timeout {
        command.env("AGENTSHIM_IDLE_TIMEOUT", timeout);
    }
    command.output().expect("run doctor")
}

#[test]
fn invalid_values_fail_startup_and_doctor_reports_profile_gating() {
    for value in ["0", "86401", "many", "-1"] {
        let output = doctor("codex", Some(value));
        assert!(!output.status.success(), "{value} must be rejected");
        assert!(String::from_utf8_lossy(&output.stderr).contains("AGENTSHIM_IDLE_TIMEOUT"));
    }

    let codex = doctor("codex", Some("7"));
    assert!(codex.status.success());
    assert!(String::from_utf8_lossy(&codex.stdout).contains("idle timeout: 7s"));

    let cursor = doctor("cursor", Some("7"));
    assert!(cursor.status.success());
    assert!(String::from_utf8_lossy(&cursor.stdout).contains("idle timeout: disabled"));

    let removed = doctor("dsh", Some("7"));
    assert!(!removed.status.success());
    assert!(String::from_utf8_lossy(&removed.stderr).contains("codex` or `cursor"));

    let disabled = doctor("codex", None);
    assert!(disabled.status.success());
    assert!(String::from_utf8_lossy(&disabled.stdout).contains("idle timeout: disabled"));
}

#[test]
fn codex_profile_exits_cleanly_after_handshake_goes_idle() {
    let mut session = TestSession::builder()
        .root(env!("CARGO_MANIFEST_DIR"))
        .profile("codex")
        .idle_timeout_secs(1)
        .log_mode("off")
        .spawn();
    session.handshake();

    let status = session.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "server exited with {status}");
}

#[test]
fn cursor_profile_ignores_the_idle_timeout() {
    let mut session = TestSession::builder()
        .root(env!("CARGO_MANIFEST_DIR"))
        .profile("cursor")
        .idle_timeout_secs(1)
        .log_mode("off")
        .spawn();
    session.handshake();

    session.assert_alive_for(Duration::from_secs(3));
}

#[test]
fn inbound_pings_keep_the_server_alive_until_they_stop() {
    let mut session = TestSession::builder()
        .root(env!("CARGO_MANIFEST_DIR"))
        .profile("codex")
        .idle_timeout_secs(2)
        .log_mode("off")
        .spawn();
    session.handshake();

    for id in 2..=8 {
        thread::sleep(Duration::from_millis(500));
        session.send(&modern_request(id, "ping", empty_params()));
        assert_eq!(session.receive()["id"], id);
    }
    assert!(session.child.try_wait().expect("poll server").is_none());

    let status = session.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "server exited with {status}");
}

#[test]
fn a_live_detached_tree_defers_idle_shutdown() {
    if agentshim::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let mut session = TestSession::builder()
        .root(fixture.path())
        .profile("codex")
        .idle_timeout_secs(1)
        .log_mode("off")
        .spawn();
    session.handshake();
    let response = session.call_tool(
        2,
        "bash",
        json!({
            "command": "sleep 5",
            "detach": true,
            "log_path": "idle-detached.log"
        }),
    );
    assert_eq!(response["result"]["isError"], false, "{response}");

    session.assert_alive_for(Duration::from_secs(3));
    let status = session.wait_for_exit(Duration::from_secs(10));

    assert!(status.success(), "server exited with {status}");
}
