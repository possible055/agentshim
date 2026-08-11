use super::support::*;
use super::*;

#[test]
fn bash_toolchain_commands_work_over_real_stdio() {
    if codexshim::bash_report().is_err() {
        return;
    }
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let response = call_tool(
        &mut session,
        2,
        "bash",
        json!({
            "command": "sleep 0.05; printf 'needle\\n' | grep needle | sed 's/needle/toolchain-ok/'; locale >/dev/null"
        }),
    );

    assert_eq!(response["result"]["isError"], false);
    assert!(response_text(&response).contains("toolchain-ok"));
    assert!(response_text(&response).contains("Exit code: 0"));
    session.close();
}

#[test]
fn detached_roster_saturation_fails_before_blocking_scheduling_over_stdio() {
    if codexshim::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let mut session = Session::start_for_bash(fixture.path(), 1, None);
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    let first = call_tool(
        &mut session,
        2,
        "bash",
        json!({
            "command": "sleep 30",
            "detach": true,
            "log_path": "first.log"
        }),
    );
    assert_eq!(first["result"]["isError"], false);

    let started = Instant::now();
    let second = call_tool(
        &mut session,
        3,
        "bash",
        json!({
            "command": "sleep 30",
            "detach": true,
            "log_path": "second.log"
        }),
    );

    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(second["result"]["isError"], true);
    assert_eq!(
        second["result"]["structuredContent"]["error"]["code"],
        "resource_busy"
    );
    assert!(response_text(&second).contains("first.log"));
    assert!(response_text(&second).contains("pid "));
    session.close();
}

#[test]
fn missing_bash_is_non_retryable_over_real_stdio() {
    let fixture = tempfile::tempdir().expect("fixture");
    let missing = fixture.path().join("missing-bash");
    let mut session = Session::start_for_bash(fixture.path(), 1, Some(&missing));
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let response = call_tool(&mut session, 2, "bash", json!({ "command": "true" }));

    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"],
        "io"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["retryable"],
        false
    );
    assert!(response_text(&response).contains("CODEXSHIM_BASH"));
    session.close();
}
