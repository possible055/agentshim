use super::common::{fixtures::*, session::*};
use super::*;

/// L1: process shutdown starts at the global cancellation — the EOF observed on stdin —
/// and runs in parallel with the protocol drain, so detached trees die inside the shared
/// shutdown deadline even while responses are still pending on a blocked stdout pipe.
#[test]
#[ignore = "local stress: saturates stdout while shutdown races a blocked protocol drain"]
fn stdin_eof_terminates_detached_trees_while_the_drain_is_still_blocked() {
    if agentshim::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let mut session = TestSession::builder()
        .root(fixture.path())
        .output_bytes(262_144)
        .burst_tokens(32768)
        .detached_calls(4)
        .spawn();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let detached = session.call_tool(
        2,
        "bash",
        json!({
            "command": "while :; do printf x >> eof-marker; sleep 0.1; done",
            "detach": true,
            "log_path": "eof.log"
        }),
    );
    assert_eq!(detached["result"]["isError"], false);
    let marker = fixture.path().join("eof-marker");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::fs::read_to_string(&marker).is_ok_and(|body| !body.is_empty()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .is_empty(),
        "the detached tree did not start"
    );

    // Four concurrent foreground responses, each truncated only at the per-call token
    // ceiling, total more than the stdout pipe holds. Nothing reads them, so the rmcp
    // drain stays pending after EOF.
    for id in 3..7 {
        session.send(&modern_request(id, "tools/call", {
            let mut call = empty_params();
            call.insert("name".to_owned(), json!("bash"));
            call.insert(
                "arguments".to_owned(),
                json!({ "command": "head -c 250000 /dev/zero | tr '\\0' x" }),
            );
            call
        }));
    }
    std::thread::sleep(Duration::from_millis(1_000));
    session.stdin.take();
    let eof = Instant::now();
    std::thread::sleep(Duration::from_secs(2));

    let observed = std::fs::read_to_string(&marker).unwrap_or_default().len();
    std::thread::sleep(Duration::from_millis(750));
    let after = std::fs::read_to_string(&marker).unwrap_or_default().len();
    assert_eq!(
        after, observed,
        "the detached tree kept running {observed} bytes past an EOF observed two seconds ago"
    );

    let status = loop {
        if let Some(status) = session.child.try_wait().expect("poll server") {
            break status;
        }
        if Instant::now() >= eof + Duration::from_secs(10) {
            let _ = session.child.kill();
            panic!("server did not exit after the drain unblocked");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "server exited with {status}");
}

/// L4/L5: every tree a shutdown owns shares one deadline. Sixteen live trees must not
/// turn into sixteen serialized five-second waits.
#[test]
#[ignore = "local stress: launches sixteen detached process trees"]
fn shutdown_of_sixteen_detached_trees_shares_one_deadline() {
    if agentshim::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let mut session = TestSession::builder()
        .root(fixture.path())
        .detached_calls(16)
        .spawn();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    for index in 0..16 {
        let response = session.call_tool(
            2 + index,
            "bash",
            json!({
                "command": "sleep 300",
                "detach": true,
                "log_path": format!("{index}.log")
            }),
        );
        assert_eq!(response["result"]["isError"], false, "tree {index}");
    }

    let eof = Instant::now();
    session.stdin.take();
    let status = loop {
        if let Some(status) = session.child.try_wait().expect("poll server") {
            break status;
        }
        if Instant::now() >= eof + Duration::from_secs(10) {
            let _ = session.child.kill();
            panic!("shutdown of sixteen trees exceeded the shared deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(status.success(), "server exited with {status}");
    assert!(
        eof.elapsed() < Duration::from_secs(8),
        "sixteen trees took {} ms to shut down",
        eof.elapsed().as_millis()
    );
}

/// `--noprofile --norc` does not stop non-interactive bash from sourcing `BASH_ENV`, so the
/// inherited environment must strip it (and its POSIX-mode twin `ENV`) before the shell —
/// including the discovery probe — ever sees it.
#[test]
fn bash_env_and_env_are_not_sourced_by_foreground_or_detached_bash() {
    if agentshim::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    std::fs::write(
        fixture.path().join("env.sh"),
        "printf sourced >> bash-env-marker\n",
    )
    .expect("env script");
    let mut session = TestSession::builder()
        .root(fixture.path())
        .env("BASH_ENV", "env.sh")
        .env("ENV", "env.sh")
        .spawn();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let foreground = session.call_tool(2, "bash", json!({ "command": "true" }));
    assert_eq!(foreground["result"]["isError"], false);
    let detached = session.call_tool(
        3,
        "bash",
        json!({
            "command": "true",
            "detach": true,
            "log_path": "detached.log"
        }),
    );
    assert_eq!(detached["result"]["isError"], false);
    std::thread::sleep(Duration::from_millis(750));

    assert!(
        !fixture.path().join("bash-env-marker").exists(),
        "BASH_ENV was sourced by a --noprofile --norc shell"
    );
    session.close();
}

#[test]
fn detached_roster_saturation_fails_before_blocking_scheduling_over_stdio() {
    if agentshim::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let mut session = TestSession::builder()
        .root(fixture.path())
        .detached_calls(1)
        .spawn();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    let first = session.call_tool(
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
    let second = session.call_tool(
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
fn detached_job_status_and_termination_work_over_real_stdio_at_capacity() {
    if agentshim::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let mut session = TestSession::builder()
        .root(fixture.path())
        .detached_calls(1)
        .spawn();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    let detached = session.call_tool(
        2,
        "bash",
        json!({
            "command": "while :; do printf x >> marker; sleep 0.05; done",
            "detach": true,
            "log_path": "managed.log"
        }),
    );
    assert_eq!(detached["result"]["isError"], false);
    let detached_text = response_text(&detached);
    let job_id = detached_text
        .split_whitespace()
        .find_map(|part| part.strip_prefix("job_id="))
        .expect("job_id")
        .to_owned();

    let status = session.call_tool(
        3,
        "bash_status",
        json!({ "job_id": job_id.clone(), "tail_bytes": 0 }),
    );
    assert_eq!(status["result"]["isError"], false);
    assert!(response_text(&status).contains("State: running"));
    assert!(response_text(&status).contains("Exit: pending"));

    let terminated = session.call_tool(
        4,
        "bash",
        json!({ "action": "terminate", "job_id": job_id.clone() }),
    );
    assert_eq!(terminated["result"]["isError"], false);
    assert!(response_text(&terminated).contains("State: terminated"));
    assert!(response_text(&terminated).contains("Outcome: verified"));

    let marker = fixture.path().join("marker");
    let observed = std::fs::metadata(&marker).map_or(0, |metadata| metadata.len());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        std::fs::metadata(&marker).map_or(0, |metadata| metadata.len()),
        observed,
        "terminated process tree kept writing"
    );
    let terminal = session.call_tool(
        5,
        "bash_status",
        json!({ "job_id": job_id, "tail_bytes": 16384 }),
    );
    assert_eq!(terminal["result"]["isError"], false);
    assert!(response_text(&terminal).contains("State: terminated"));

    let unknown = session.call_tool(
        6,
        "bash_status",
        json!({ "job_id": format!("bash-{}", uuid::Uuid::new_v4()) }),
    );
    assert_eq!(unknown["result"]["isError"], true);
    assert_eq!(
        unknown["result"]["structuredContent"]["error"]["code"],
        "validation"
    );
    assert_eq!(
        unknown["result"]["structuredContent"]["error"]["retryable"],
        false
    );
    session.close();
}

#[test]
fn missing_bash_is_non_retryable_over_real_stdio() {
    let fixture = tempfile::tempdir().expect("fixture");
    let missing = fixture.path().join("missing-bash");
    let mut session = TestSession::builder()
        .root(fixture.path())
        .detached_calls(1)
        .bash_override(&missing)
        .spawn();
    session.send(&modern_request(1, "server/discover", empty_params()));
    let status = session.wait_for_exit(Duration::from_secs(10));
    assert!(
        !status.success(),
        "server must exit non-zero when GNU bash is unavailable at startup"
    );
}
