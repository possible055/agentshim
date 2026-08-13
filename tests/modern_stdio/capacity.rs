use super::process::process_is_running;
use super::support::*;
use super::*;

fn projected_success_tokens(responses: &[Value]) -> usize {
    let prototype = codexshim_gigatoken::O200kPrototype::load_embedded().expect("token ranks");
    let mut counter = prototype
        .fork_counter(codexshim_gigatoken::CounterLimits::default())
        .expect("counter");
    responses
        .iter()
        .filter(|response| response["result"]["isError"] == false)
        .map(|response| {
            let payload =
                serde_json::to_string(&response["result"]["content"]).expect("content JSON");
            let codexshim_gigatoken::CountUpTo::Exact(tokens) =
                counter.count_ordinary_up_to(&payload, usize::MAX, || false)
            else {
                panic!("unbounded exact count")
            };
            128 + tokens
        })
        .sum()
}

#[test]
fn parallel_large_reads_share_one_projected_burst_budget() {
    const CALLS: u64 = 16;
    let fixture = tempfile::tempdir().expect("fixture");
    let body = (0..4_000)
        .map(|line| format!("{line} {}", " x".repeat(20)))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(fixture.path().join("large.txt"), body).expect("large fixture");
    let mut session = Session::start_at(fixture.path());
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    for id in 2..2 + CALLS {
        let mut call = empty_params();
        call.insert("name".to_owned(), json!("read"));
        call.insert(
            "arguments".to_owned(),
            json!({ "path": "large.txt", "line_count": 1000 }),
        );
        session.send(&modern_request(id, "tools/call", call));
    }
    let responses = (0..CALLS).map(|_| session.receive()).collect::<Vec<_>>();
    assert!(
        projected_success_tokens(&responses) <= 8_192,
        "content-bearing responses exceeded the shared burst budget"
    );
    assert!(responses.iter().all(|response| {
        response["result"]["isError"] == false
            || response["result"]["structuredContent"]["error"]["code"] == "output_budget"
    }));
    session.close();
}

#[test]
fn process_overload_is_fail_fast_and_preserves_resource_busy_contract() {
    let fixture = tempfile::tempdir().expect("fixture");
    let executable = std::env::current_exe().expect("integration test executable");
    let mut session = Session::start_with_process_calls(2);
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let mut pid_files = Vec::new();
    for id in [2_u64, 3_u64] {
        let pid_file = fixture.path().join(format!("overload-child-{id}.pid"));
        let mut call = empty_params();
        call.insert("name".to_owned(), json!("run_program"));
        call.insert(
            "arguments".to_owned(),
            json!({
                "program": executable,
                "args": ["--exact", "process::eof_process_child_fixture", "--nocapture"],
                "cwd": env!("CARGO_MANIFEST_DIR"),
                "env": {
                    "CODEXSHIM_EOF_FIXTURE": "child",
                    "CODEXSHIM_EOF_PID_FILE": pid_file,
                },
                "timeout_ms": 30_000,
            }),
        );
        session.send(&modern_request(id, "tools/call", call));
        pid_files.push(pid_file);
    }
    let active_deadline = Instant::now() + Duration::from_secs(5);
    while pid_files.iter().any(|path| !path.exists()) && Instant::now() < active_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        pid_files.iter().all(|path| path.exists()),
        "two process calls did not occupy the documented class capacity"
    );

    let mut overflow = empty_params();
    overflow.insert("name".to_owned(), json!("run_program"));
    overflow.insert(
        "arguments".to_owned(),
        json!({
            "program": executable,
            "args": ["--exact", "process::eof_process_child_fixture", "--nocapture"],
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "timeout_ms": 30_000,
        }),
    );
    let started = Instant::now();
    session.send(&modern_request(4, "tools/call", overflow));
    let response = session.receive();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "overload response waited for process capacity"
    );
    assert_eq!(response["id"], 4);
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"],
        "resource_busy"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["retryable"],
        true
    );
    session.close();

    for pid_file in pid_files {
        let pid = std::fs::read_to_string(pid_file)
            .expect("child PID")
            .trim()
            .parse::<u32>()
            .expect("numeric child PID");
        assert!(
            !process_is_running(pid),
            "cancelled overload fixture survived"
        );
    }
}

#[test]
fn default_process_and_read_only_capacity_can_progress_together() {
    const CAPACITY: u64 = 16;

    let fixture = tempfile::tempdir().expect("fixture");
    let executable = std::env::current_exe().expect("integration test executable");
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let mut pid_files = Vec::new();
    for id in 2..2 + CAPACITY {
        let pid_file = fixture.path().join(format!("parallel-child-{id}.pid"));
        let mut call = empty_params();
        call.insert("name".to_owned(), json!("run_program"));
        call.insert(
            "arguments".to_owned(),
            json!({
                "program": executable,
                "args": ["--exact", "process::eof_process_child_fixture", "--nocapture"],
                "cwd": env!("CARGO_MANIFEST_DIR"),
                "env": {
                    "CODEXSHIM_EOF_FIXTURE": "child",
                    "CODEXSHIM_EOF_PID_FILE": pid_file,
                },
                "timeout_ms": 30_000,
            }),
        );
        session.send(&modern_request(id, "tools/call", call));
        pid_files.push(pid_file);
    }
    let active_deadline = Instant::now() + Duration::from_secs(10);
    while pid_files.iter().any(|path| !path.exists()) && Instant::now() < active_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        pid_files.iter().all(|path| path.exists()),
        "sixteen process calls did not start concurrently"
    );

    let read_ids = (100..100 + CAPACITY).collect::<BTreeSet<_>>();
    for id in &read_ids {
        let mut call = empty_params();
        call.insert("name".to_owned(), json!("read"));
        call.insert(
            "arguments".to_owned(),
            json!({ "path": "Cargo.toml", "line_count": 1 }),
        );
        session.send(&modern_request(*id, "tools/call", call));
    }
    let mut completed_read_ids = BTreeSet::new();
    for _ in 0..CAPACITY {
        let response = session.receive();
        let id = response["id"].as_u64().expect("response id");
        assert!(read_ids.contains(&id), "unexpected response id {id}");
        assert_eq!(response["result"]["isError"], false);
        completed_read_ids.insert(id);
    }
    assert_eq!(completed_read_ids, read_ids);

    let mut overflow = empty_params();
    overflow.insert("name".to_owned(), json!("run_program"));
    overflow.insert(
        "arguments".to_owned(),
        json!({
            "program": executable,
            "args": ["--version"],
            "cwd": env!("CARGO_MANIFEST_DIR"),
        }),
    );
    session.send(&modern_request(200, "tools/call", overflow));
    let response = session.receive();
    assert_eq!(response["id"], 200);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"],
        "resource_busy"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["retryable"],
        true
    );

    session.close();
    for pid_file in pid_files {
        let pid = std::fs::read_to_string(pid_file)
            .expect("child PID")
            .trim()
            .parse::<u32>()
            .expect("numeric child PID");
        assert!(
            !process_is_running(pid),
            "parallel fixture survived server shutdown"
        );
    }
}
