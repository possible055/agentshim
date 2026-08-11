use super::pending::request_slope;
use super::support::*;
use super::*;

#[test]
fn pending_process_child_fixture() {
    if env::var("CODEXSHIM_PENDING_FIXTURE").as_deref() != Ok("child") {
        return;
    }
    let duration =
        env::var("CODEXSHIM_PENDING_FIXTURE_MS").map_or(Duration::from_secs(30), |value| {
            Duration::from_millis(
                value
                    .parse()
                    .expect("CODEXSHIM_PENDING_FIXTURE_MS must be an integer"),
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

    let iterations = env::var("CODEXSHIM_AGGREGATE_SOAK_ITERATIONS").map_or(5, |value| {
        value
            .parse::<usize>()
            .expect("CODEXSHIM_AGGREGATE_SOAK_ITERATIONS must be a positive integer")
    });
    assert!(iterations > 0, "aggregate soak requires an iteration");
    let output = env::var_os("CODEXSHIM_AGGREGATE_SOAK_OUTPUT").map_or_else(
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
    let mut samples = Vec::new();

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
                            "CODEXSHIM_PENDING_FIXTURE": "child",
                            "CODEXSHIM_PENDING_FIXTURE_MS": "750",
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
            assert!(
                Instant::now() < peak_deadline,
                "aggregate children did not reach expected peak"
            );
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

        for session in &mut sessions {
            for _ in 0..CALLS_PER_INSTANCE {
                let response = session.receive_any();
                assert_eq!(
                    response["result"]["isError"], false,
                    "aggregate process failed: {response}"
                );
            }
        }
        let settled = sessions
            .iter()
            .map(|session| platform::sample(session.pid()).expect("sample settled server"))
            .collect::<Vec<_>>();
        assert!(
            settled.iter().all(|sample| sample.descendants.is_empty()),
            "aggregate descendants survived completion"
        );
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
        samples.push((iteration, memory_bytes));
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
    }

    let server_exit_statuses = sessions.into_iter().map(Session::close).collect::<Vec<_>>();
    let initial_memory = samples.first().expect("initial aggregate sample").1;
    let final_memory = samples.last().expect("final aggregate sample").1;
    let tail_sample_count = samples.len().min(10);
    let tail_samples = &samples[samples.len() - tail_sample_count..];
    let tail_initial_memory = tail_samples.first().expect("initial tail sample").1;
    let tail_final_memory = tail_samples.last().expect("final tail sample").1;
    artifact.write(&json!({
        "record_type": "result",
        "outcome": "measured",
        "server_exit_statuses": server_exit_statuses,
        "initial_memory_bytes": initial_memory,
        "final_memory_bytes": final_memory,
        "retained_growth_bytes": final_memory.saturating_sub(initial_memory),
        "least_squares_bytes_per_iteration": request_slope(&samples),
        "tail_sample_count": tail_sample_count,
        "tail_net_growth_bytes": i64::try_from(tail_final_memory).expect("memory fits i64")
            - i64::try_from(tail_initial_memory).expect("memory fits i64"),
        "tail_least_squares_bytes_per_iteration": request_slope(tail_samples),
        "surviving_descendants": [],
        "threshold_policy": "zero surviving descendants is blocking; full and tail growth slopes are release evidence",
        "finished_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));
    println!("aggregate soak artifact: {}", artifact.path.display());
}
