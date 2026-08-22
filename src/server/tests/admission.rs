use super::*;

fn detached_request() -> CallToolRequestParams {
    serde_json::from_value(json!({
        "name": "bash",
        "arguments": {
            "command": "sleep 30",
            "detach": true,
            "log_path": "build.log"
        }
    }))
    .expect("call tool request")
}

fn bash_request(command: &str) -> CallToolRequestParams {
    serde_json::from_value(json!({
        "name": "bash",
        "arguments": { "command": command }
    }))
    .expect("bash request")
}

fn bash_terminate_request() -> CallToolRequestParams {
    serde_json::from_value(json!({
        "name": "bash",
        "arguments": {
            "action": "terminate",
            "job_id": format!("bash-{}", uuid::Uuid::new_v4())
        }
    }))
    .expect("bash terminate request")
}

fn bash_status_request() -> CallToolRequestParams {
    serde_json::from_value(json!({
        "name": "bash_status",
        "arguments": { "job_id": format!("bash-{}", uuid::Uuid::new_v4()) }
    }))
    .expect("bash status request")
}

#[test]
fn shell_delegate_classifies_only_the_first_token_file_stem() {
    for (command, expected) in [
        ("pwsh -NoProfile -File release.ps1", "pwsh"),
        (
            r#""C:\Program Files\PowerShell\7\powershell.exe" -Command x"#,
            "pwsh",
        ),
        ("cmd.exe /c ver", "cmd"),
        (r"C:\Windows\System32\wsl.exe --status", "wsl"),
        (r"C:\Windows\System32\bash.exe -lc true", "wsl"),
        ("python.exe -c pass", "other-interpreter"),
        ("node script.js", "other-interpreter"),
        ("git pwsh -Command Get-Process", "none"),
        ("bash -lc true", "none"),
    ] {
        assert_eq!(
            shell_delegate(&bash_request(command)),
            expected,
            "{command}"
        );
    }
}

#[test]
fn detached_admission_reserves_before_blocking_scheduling_and_fails_fast() {
    let fixture = tempfile::tempdir().expect("fixture");
    let mut runtime = crate::runtime::RuntimeConfig::for_tests(1);
    runtime.detached_calls = 1;
    let server = AgentShim::builder(fixture.path())
        .expect("builder")
        .runtime_limits(runtime)
        .build()
        .expect("server");
    let request = detached_request();
    let first = server
        .try_admit_tool(&request)
        .expect("first detached admission");

    assert_eq!(server.detached.reserved_count(), 1);
    assert!(matches!(
        server.try_admit_tool(&request),
        Err(crate::tools::exec::ProcessError::ResourceBusy(_))
    ));
    drop(first);
    assert_eq!(server.detached.reserved_count(), 0);
}

#[test]
fn foreground_saturation_does_not_consume_detached_capacity() {
    let fixture = tempfile::tempdir().expect("fixture");
    let mut runtime = crate::runtime::RuntimeConfig::for_tests(1);
    runtime.process_calls = 1;
    runtime.detached_calls = 1;
    let server = AgentShim::builder(fixture.path())
        .expect("builder")
        .runtime_limits(runtime)
        .build()
        .expect("server");
    let foreground = server
        .resources
        .try_admit_process_for_test()
        .expect("foreground admission");

    assert!(server.resources.try_admit_process_for_test().is_none());
    let detached = server
        .try_admit_tool(&detached_request())
        .expect("detached admission remains independent");
    assert!(matches!(detached, ToolAdmission::Detached(_)));
    drop(foreground);
}

#[test]
fn detached_control_bypasses_process_and_detached_capacity() {
    let fixture = tempfile::tempdir().expect("fixture");
    let mut runtime = crate::runtime::RuntimeConfig::for_tests(1);
    runtime.process_calls = 1;
    runtime.detached_calls = 1;
    let server = AgentShim::builder(fixture.path())
        .expect("builder")
        .runtime_limits(runtime)
        .build()
        .expect("server");
    let _foreground = server
        .resources
        .try_admit_process_for_test()
        .expect("foreground admission");
    let _detached = server.detached.admit().expect("detached reservation");

    assert!(matches!(
        server
            .try_admit_tool(&bash_terminate_request())
            .expect("detached control"),
        ToolAdmission::DetachedControl
    ));
    assert!(matches!(
        server
            .try_admit_tool(&bash_status_request())
            .expect("status admission"),
        ToolAdmission::AuxiliaryReadOnly
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_after_detached_commit_preserves_the_job_id_response() {
    if crate::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let server = AgentShim::from_path(fixture.path()).expect("server");
    let request = detached_request();
    let admission = server.try_admit_tool(&request).expect("detached admission");
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let hook_entered = Arc::clone(&entered);
    let hook_release = Arc::clone(&release);
    server.detached.set_after_commit_hook(move || {
        hook_entered.wait();
        hook_release.wait();
    });
    let cancellation = tokio_util::sync::CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let budget = default_output_budget();
    let worker_budget = budget.clone();
    let worker_server = server.clone();
    let worker = tokio::spawn(async move {
        worker_server
            .call_bash_for_test(
                request.arguments,
                &worker_cancellation,
                admission,
                &worker_budget,
            )
            .await
    });

    entered.wait();
    cancellation.cancel();
    release.wait();
    let response = worker.await.expect("detached worker");
    let response = finalize_tool_response("bash", &budget, Ok(response), &cancellation)
        .expect("final response");
    let CallToolResponse::Complete(result) = response else {
        panic!("detached response must be complete");
    };
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("detached response must contain text");
    };
    let job_id = content
        .text
        .split_whitespace()
        .find_map(|part| part.strip_prefix("job_id="))
        .expect("detached job id");

    assert_eq!(result.is_error, Some(false));
    assert!(server.detached.status(job_id, 0).is_ok());
    server.detached.terminate_all();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_request_future_after_commit_still_arms_the_deadline() {
    if crate::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let server = AgentShim::from_path(fixture.path()).expect("server");
    let request: CallToolRequestParams = serde_json::from_value(json!({
        "name": "bash",
        "arguments": {
            "command": "while :; do sleep 0.02; done",
            "detach": true,
            "log_path": "dropped-request.log",
            "timeout_ms": 100
        }
    }))
    .expect("timeout request");
    let admission = server.try_admit_tool(&request).expect("detached admission");
    let job_id = match &admission {
        ToolAdmission::Detached(admission) => admission.job_id().to_owned(),
        _ => panic!("detached request must reserve detached admission"),
    };
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let hook_entered = Arc::clone(&entered);
    let hook_release = Arc::clone(&release);
    server.detached.set_after_commit_hook(move || {
        hook_entered.wait();
        hook_release.wait();
    });
    let worker_server = server.clone();
    let worker = tokio::spawn(async move {
        worker_server
            .call_bash_for_test(
                request.arguments,
                &tokio_util::sync::CancellationToken::new(),
                admission,
                &default_output_budget(),
            )
            .await
    });

    entered.wait();
    worker.abort();
    release.wait();
    let _ = worker.await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let snapshot = server
        .detached
        .status(&job_id, 0)
        .expect("timed out status");
    assert_eq!(
        snapshot.state,
        crate::tools::bash::status::JobState::TimedOut
    );
    assert_eq!(snapshot.cause, Some("timeout"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_timeout_stops_the_tree_without_status_polling() {
    if crate::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let server = AgentShim::from_path(fixture.path()).expect("server");
    let request: CallToolRequestParams = serde_json::from_value(json!({
        "name": "bash",
        "arguments": {
            "command": "while :; do printf x >> marker.txt; sleep 0.02; done",
            "detach": true,
            "log_path": "timeout.log",
            "timeout_ms": 100
        }
    }))
    .expect("timeout request");
    let admission = server.try_admit_tool(&request).expect("detached admission");
    let budget = default_output_budget();
    let response = server
        .call_bash_for_test(
            request.arguments,
            &tokio_util::sync::CancellationToken::new(),
            admission,
            &budget,
        )
        .await;
    let CallToolResponse::Complete(result) = response else {
        panic!("detached response must be complete");
    };
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("detached response must contain text");
    };
    let job_id = content
        .text
        .split_whitespace()
        .find_map(|part| part.strip_prefix("job_id="))
        .expect("detached job id");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let first_len = fs::metadata(fixture.path().join("marker.txt"))
        .expect("marker")
        .len();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let second_len = fs::metadata(fixture.path().join("marker.txt"))
        .expect("stable marker")
        .len();
    let snapshot = server.detached.status(job_id, 0).expect("timed out status");
    assert_eq!(
        snapshot.state,
        crate::tools::bash::status::JobState::TimedOut
    );
    assert_eq!(snapshot.cause, Some("timeout"));
    assert_eq!(
        first_len, second_len,
        "process tree kept writing after timeout"
    );
}

#[test]
fn root_capability_blocks_parent_escape() {
    let fixture = tempfile::tempdir().expect("create fixture");
    let root = fixture.path().join("root");
    fs::create_dir(&root).expect("create root");
    fs::write(fixture.path().join("outside.txt"), "outside").expect("write outside");
    let server = AgentShim::from_path(&root).expect("open root");

    let error = server
        .root
        .capability()
        .read_to_string("../outside.txt")
        .expect_err("parent escape must fail");
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
    ));
}

#[cfg(unix)]
#[test]
fn root_capability_blocks_symlink_escape() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("create fixture");
    let root = fixture.path().join("root");
    fs::create_dir(&root).expect("create root");
    let outside = fixture.path().join("outside.txt");
    fs::write(&outside, "outside").expect("write outside");
    symlink(&outside, root.join("escape")).expect("create symlink");
    let server = AgentShim::from_path(&root).expect("open root");

    server
        .root
        .capability()
        .read_to_string("escape")
        .expect_err("symlink escape must fail");
}

#[cfg(any(unix, windows))]
#[test]
fn root_handle_preserves_repository_identity() {
    let fixture = tempfile::tempdir().expect("create fixture");
    let root = fixture.path().join("root");
    let moved = fixture.path().join("moved");
    fs::create_dir(&root).expect("create root");
    fs::write(root.join("identity.txt"), "original").expect("write original");
    let server = AgentShim::from_path(&root).expect("open root");

    #[cfg(unix)]
    {
        fs::rename(&root, &moved).expect("move original root");
        fs::create_dir(&root).expect("create replacement root");
        fs::write(root.join("identity.txt"), "replacement").expect("write replacement");
    }
    #[cfg(windows)]
    {
        let error = fs::rename(&root, &moved).expect_err("held Windows root blocks replacement");
        assert!(
            matches!(error.raw_os_error(), Some(5 | 32)),
            "unexpected Windows root rename error: {error}"
        );
    }

    assert_eq!(
        server
            .root
            .capability()
            .read_to_string("identity.txt")
            .expect("read held root"),
        "original"
    );
}

/// Re-entrant shutdown: concurrent callers share one transaction and one report, the
/// global token ends up cancelled, and roster admission closes for good.
#[tokio::test]
async fn shutdown_processes_is_idempotent_and_closes_admission() {
    let fixture = tempfile::tempdir().expect("fixture");
    let server = AgentShim::from_path(fixture.path()).expect("server");
    let first = server.clone();
    let second = server.clone();
    let started = std::time::Instant::now();

    let (first, second) = tokio::join!(first.shutdown_processes(), second.shutdown_processes());
    let _ = (first, second);

    assert!(server.shutdown_token().is_cancelled());
    assert!(!server.detached.is_accepting());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(6),
        "overlapping shutdown callers each ran their own cleanup"
    );
}

/// The shutdown transaction waits for foreground owners inside its shared deadline: a
/// held process permit keeps it pending, and releasing the permit lets it finish.
#[tokio::test]
async fn shutdown_waits_for_foreground_owners_to_release() {
    let fixture = tempfile::tempdir().expect("fixture");
    let server = AgentShim::from_path(fixture.path()).expect("server");
    let permit = server
        .resources
        .try_admit_process_for_test()
        .expect("one foreground permit");

    let shutdown = server.clone();
    let waiter = tokio::spawn(async move {
        shutdown.shutdown_processes().await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !waiter.is_finished(),
        "shutdown completed while a foreground owner still held its permit"
    );

    drop(permit);
    tokio::time::timeout(std::time::Duration::from_secs(6), waiter)
        .await
        .expect("shutdown completed after the foreground owner released")
        .expect("shutdown task");
}
