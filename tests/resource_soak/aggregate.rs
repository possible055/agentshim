use super::pending::request_slope;
use super::support::*;
use super::*;

#[test]
fn pending_process_child_fixture() {
    if env::var("AGENTSHIM_PENDING_FIXTURE").as_deref() != Ok("child") {
        return;
    }
    let duration =
        env::var("AGENTSHIM_PENDING_FIXTURE_MS").map_or(Duration::from_secs(30), |value| {
            Duration::from_millis(
                value
                    .parse()
                    .expect("AGENTSHIM_PENDING_FIXTURE_MS must be an integer"),
            )
        });
    thread::sleep(duration);
}

#[test]
#[ignore = "manual four-instance aggregate process soak; run with --ignored --nocapture"]
#[allow(clippy::too_many_lines)] // The ignored fixture records one complete aggregate soak scenario.
fn four_instance_aggregate_process_soak() {
    const INSTANCE_COUNT: usize = 4;
    const CALLS_PER_INSTANCE: usize = 16;

    let iterations = env::var("AGENTSHIM_AGGREGATE_SOAK_ITERATIONS").map_or(5, |value| {
        value
            .parse::<usize>()
            .expect("AGENTSHIM_AGGREGATE_SOAK_ITERATIONS must be a positive integer")
    });
    assert!(iterations > 0, "aggregate soak requires an iteration");
    let output = env::var_os("AGENTSHIM_AGGREGATE_SOAK_OUTPUT").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("resource-soak")
                .join(format!("{}-four-instance.jsonl", env::consts::OS))
        },
        PathBuf::from,
    );
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).expect("create aggregate soak artifact directory");
    }
    let mut artifact = Artifact {
        writer: BufWriter::new(File::create(&output).expect("create aggregate soak artifact")),
        path: output,
    };
    artifact.write(&json!({
        "record_type": "metadata",
        "schema_version": 1,
        "scenario": "four_instance_process_aggregate",
        "platform": env::consts::OS,
        "architecture": env::consts::ARCH,
        "commit": command_output("git", &["rev-parse", "HEAD"]),
        "worktree_status": command_output("git", &["status", "--short"]),
        "toolchain": command_output("cargo", &["--version", "--verbose"]),
        "runner_image": runner_image(),
        "instances": INSTANCE_COUNT,
        "process_calls_per_instance": CALLS_PER_INSTANCE,
        "aggregate_process_calls": INSTANCE_COUNT * CALLS_PER_INSTANCE,
        "iterations": iterations,
        "workload_iterations": iterations,
        "measured_iterations": iterations,
        "burst_epoch_policy": "one aggregate batch per production burst epoch",
        "burst_quiet_ms": BURST_QUIET_MS,
        "started_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));

    let executable = env::current_exe().expect("integration test executable");
    let mut sessions = (0..INSTANCE_COUNT)
        .map(|_| {
            let mut session = Session::start();
            session.discover();
            session
        })
        .collect::<Vec<_>>();
    artifact.write(&json!({
        "record_type": "sessions",
        "server_pids": sessions.iter().map(Session::pid).collect::<Vec<_>>(),
    }));
    let mut memory_samples = Vec::new();
    let mut resource_samples = Vec::new();

    for iteration in 1..=iterations {
        for session in &mut sessions {
            for _ in 0..CALLS_PER_INSTANCE {
                session.send_tool(
                    "run_program",
                    json!({
                        "program": executable,
                        "args": ["--exact", "aggregate::pending_process_child_fixture", "--nocapture"],
                        "cwd": env!("CARGO_MANIFEST_DIR"),
                        "env": {
                            "AGENTSHIM_PENDING_FIXTURE": "child",
                            "AGENTSHIM_PENDING_FIXTURE_MS": "750",
                        },
                        "timeout_ms": 30_000,
                    }),
                );
            }
        }

        let peak_deadline = Instant::now() + Duration::from_secs(10);
        let peak = loop {
            let current = sessions
                .iter()
                .map(|session| platform::sample(session.pid()).expect("sample aggregate server"))
                .collect::<Vec<_>>();
            if current
                .iter()
                .all(|sample| sample.descendants.len() >= CALLS_PER_INSTANCE)
            {
                break current;
            }
            if Instant::now() >= peak_deadline {
                let active_descendant_pids = current
                    .into_iter()
                    .flat_map(|sample| sample.descendants)
                    .collect::<Vec<_>>();
                let server_exit_statuses =
                    sessions.drain(..).map(Session::close).collect::<Vec<_>>();
                artifact.write(&json!({
                    "record_type": "result",
                    "outcome": "fail",
                    "failure_kind": "peak_timeout",
                    "iteration": iteration,
                    "tool": "run_program",
                    "request_id": Value::Null,
                    "error_code": "aggregate_peak_timeout",
                    "error_details": Value::Null,
                    "active_descendant_pids": active_descendant_pids,
                    "server_exit_statuses": server_exit_statuses,
                    "finished_unix_ms": SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("system time")
                        .as_millis(),
                }));
                panic!("aggregate children did not reach expected peak");
            }
            thread::sleep(Duration::from_millis(10));
        };
        let peak_descendant_count = peak
            .iter()
            .map(|sample| sample.descendants.len())
            .sum::<usize>();
        assert!(
            peak_descendant_count >= INSTANCE_COUNT * CALLS_PER_INSTANCE,
            "aggregate process tree did not contain every direct child"
        );

        let mut tool_failure = None;
        'responses: for session in &mut sessions {
            for _ in 0..CALLS_PER_INSTANCE {
                let response = session.receive_any();
                if response["result"]["isError"] != false
                    || response["result"]["resultType"] != "complete"
                {
                    tool_failure = Some(ToolCallFailure::from_response(
                        "run_program",
                        response["id"].as_u64().expect("numeric request ID"),
                        response,
                    ));
                    break 'responses;
                }
            }
        }
        if let Some(failure) = tool_failure {
            let active_descendant_pids = sessions
                .iter()
                .filter_map(|session| platform::sample(session.pid()).ok())
                .flat_map(|sample| sample.descendants)
                .collect::<Vec<_>>();
            let server_exit_statuses = sessions.drain(..).map(Session::close).collect::<Vec<_>>();
            artifact.write(&json!({
                "record_type": "result",
                "outcome": "fail",
                "failure_kind": "tool",
                "iteration": iteration,
                "tool": failure.tool,
                "request_id": failure.request_id,
                "error_code": failure.code,
                "error_details": failure.details,
                "response": failure.response,
                "active_descendant_pids": active_descendant_pids,
                "server_exit_statuses": server_exit_statuses,
                "finished_unix_ms": SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system time")
                    .as_millis(),
            }));
            panic!(
                "{} request {} failed during aggregate iteration {}",
                failure.tool, failure.request_id, iteration
            );
        }
        let settled = sessions
            .iter()
            .map(|session| platform::sample(session.pid()).expect("sample settled server"))
            .collect::<Vec<_>>();
        if settled.iter().any(|sample| !sample.descendants.is_empty()) {
            let active_descendant_pids = settled
                .iter()
                .flat_map(|sample| sample.descendants.iter().copied())
                .collect::<Vec<_>>();
            let server_exit_statuses = sessions.drain(..).map(Session::close).collect::<Vec<_>>();
            artifact.write(&json!({
                "record_type": "result",
                "outcome": "fail",
                "failure_kind": "surviving_descendants",
                "iteration": iteration,
                "tool": "run_program",
                "request_id": Value::Null,
                "error_code": "controlled_descendants_survived",
                "error_details": Value::Null,
                "active_descendant_pids": active_descendant_pids,
                "server_exit_statuses": server_exit_statuses,
                "finished_unix_ms": SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system time")
                    .as_millis(),
            }));
            panic!("aggregate descendants survived completion");
        }
        let memory_bytes = settled
            .iter()
            .map(|sample| sample.resources.memory_bytes)
            .sum::<u64>();
        let virtual_memory_bytes = settled.iter().try_fold(0_u64, |total, sample| {
            sample
                .resources
                .virtual_memory_bytes
                .map(|value| total + value)
        });
        let resource_count = settled
            .iter()
            .map(|sample| sample.resources.resource_count)
            .sum::<u64>();
        let threads = settled
            .iter()
            .map(|sample| sample.resources.threads)
            .sum::<u64>();
        memory_samples.push((iteration, memory_bytes));
        resource_samples.push(ResourceSample {
            memory_bytes,
            virtual_memory_bytes,
            resource_count,
            threads,
        });
        artifact.write(&json!({
            "record_type": "sample",
            "iteration": iteration,
            "aggregate_memory_bytes": memory_bytes,
            "aggregate_virtual_memory_bytes": virtual_memory_bytes,
            "aggregate_resource_count": resource_count,
            "aggregate_threads": threads,
            "peak_descendant_count": peak_descendant_count,
            "settled_descendant_count": 0,
        }));
        wait_for_next_burst_epoch(iteration, iterations);
    }

    let server_exit_statuses = sessions.into_iter().map(Session::close).collect::<Vec<_>>();
    let initial_memory = memory_samples.first().expect("initial aggregate sample").1;
    let final_memory = memory_samples.last().expect("final aggregate sample").1;
    let tail_sample_count = memory_samples.len().min(10);
    let tail_samples = &memory_samples[memory_samples.len() - tail_sample_count..];
    let tail_initial_memory = tail_samples.first().expect("initial tail sample").1;
    let tail_final_memory = tail_samples.last().expect("final tail sample").1;
    let resource_growth = sustained_tail_growth(&resource_samples, |sample| sample.resource_count);
    let thread_growth = sustained_tail_growth(&resource_samples, |sample| sample.threads);
    let passed = server_exit_statuses.iter().all(|status| status.success)
        && !resource_growth
        && !thread_growth;
    artifact.write(&json!({
        "record_type": "result",
        "outcome": if passed { "pass" } else { "fail" },
        "server_exit_statuses": server_exit_statuses,
        "initial_memory_bytes": initial_memory,
        "final_memory_bytes": final_memory,
        "retained_growth_bytes": final_memory.saturating_sub(initial_memory),
        "least_squares_bytes_per_iteration": request_slope(&memory_samples),
        "tail_sample_count": tail_sample_count,
        "tail_net_growth_bytes": i64::try_from(tail_final_memory).expect("memory fits i64")
            - i64::try_from(tail_initial_memory).expect("memory fits i64"),
        "tail_least_squares_bytes_per_iteration": request_slope(tail_samples),
        "surviving_descendants": [],
        "resources": metric_summary(&resource_samples, |sample| sample.resource_count),
        "threads": metric_summary(&resource_samples, |sample| sample.threads),
        "resource_tail_growth_blocking": resource_growth,
        "thread_tail_growth_blocking": thread_growth,
        "threshold_policy": "zero surviving descendants is blocking; resource and thread tail growth require net growth >= 3, slope >= 0.25 per iteration, and increases in at least half of tail transitions; memory growth is observational",
        "finished_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));
    assert!(
        !resource_growth,
        "aggregate resource count grew throughout the tail"
    );
    assert!(
        !thread_growth,
        "aggregate thread count grew throughout the tail"
    );
    assert!(
        server_exit_statuses.iter().all(|status| status.success),
        "an aggregate server exited unsuccessfully"
    );
    println!("aggregate soak artifact: {}", artifact.path.display());
}
