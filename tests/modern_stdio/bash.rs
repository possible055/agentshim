use super::support::*;
use super::*;

/// L1: process shutdown starts at the global cancellation — the EOF observed on stdin —
/// and runs in parallel with the protocol drain, so detached trees die inside the shared
/// shutdown deadline even while responses are still pending on a blocked stdout pipe.
#[test]
fn stdin_eof_terminates_detached_trees_while_the_drain_is_still_blocked() {
    if agentshim::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let mut command = Session::base_command(fixture.path());
    command
        .env("AGENTSHIM_OUTPUT_BYTES", "262144")
        .env("AGENTSHIM_BURST_TOKENS", "32768")
        .env("AGENTSHIM_DETACHED_CALLS", "4");
    let mut session = Session::spawn(command);
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let detached = call_tool(
        &mut session,
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
            session.child.kill().expect("kill hung server");
            panic!("server did not exit after the drain unblocked");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "server exited with {status}");
}

/// L4/L5: every tree a shutdown owns shares one deadline. Sixteen live trees must not
/// turn into sixteen serialized five-second waits.
#[test]
fn shutdown_of_sixteen_detached_trees_shares_one_deadline() {
    if agentshim::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let mut session = Session::start_for_bash(fixture.path(), 16, None);
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    for index in 0..16 {
        let response = call_tool(
            &mut session,
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
            session.child.kill().expect("kill hung server");
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
    let mut command = Session::base_command(fixture.path());
    command.env("BASH_ENV", "env.sh").env("ENV", "env.sh");
    let mut session = Session::spawn(command);
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let foreground = call_tool(&mut session, 2, "bash", json!({ "command": "true" }));
    assert_eq!(foreground["result"]["isError"], false);
    let detached = call_tool(
        &mut session,
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
    assert!(response_text(&response).contains("AGENTSHIM_BASH"));
    session.close();
}
