use super::support::*;
use super::*;

#[test]
#[ignore = "manual resource soak; run with --ignored --nocapture"]
#[allow(clippy::too_many_lines)] // The ignored fixture records one complete mixed soak scenario.
fn mixed_workload_resource_soak() {
    let iterations = iteration_count();
    assert!(
        iterations >= 2,
        "resource soak requires at least two iterations"
    );
    let warm_up = warm_up_count(iterations);
    assert!(
        warm_up < iterations,
        "warm-up must be shorter than the soak"
    );

    let commit = command_output("git", &["rev-parse", "HEAD"]);
    let worktree = command_output("git", &["status", "--short"]);
    let production_worktree = command_output(
        "git",
        &["status", "--short", "--", "src", "Cargo.toml", "Cargo.lock"],
    );
    let toolchain = command_output("cargo", &["--version", "--verbose"]);
    let mut artifact = Artifact::create();
    artifact.write(&json!({
        "record_type": "metadata",
        "schema_version": 1,
        "scenario": "mixed_workload",
        "platform": env::consts::OS,
        "architecture": env::consts::ARCH,
        "commit": commit,
        "worktree_status": worktree,
        "production_server_unchanged": production_worktree.is_empty(),
        "production_worktree_status": production_worktree,
        "toolchain": toolchain,
        "runner_image": runner_image(),
        "iterations": iterations,
        "warm_up_iterations": warm_up,
        "measured_iterations": iterations - warm_up,
        "workload_iterations": iterations,
        "sample_unit": "one sequential read, glob, grep, and run_program cycle",
        "burst_epoch_policy": "one workload iteration per production burst epoch",
        "burst_quiet_ms": BURST_QUIET_MS,
        "started_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));

    let mut session = Session::start();
    artifact.write(&json!({
        "record_type": "session",
        "server_pid": session.pid(),
    }));
    session.discover();
    let mut samples = Vec::with_capacity(iterations);
    let mut surviving_descendants = BTreeMap::<usize, Vec<u32>>::new();
    for iteration in 1..=iterations {
        let started = Instant::now();
        let outcomes = match run_mixed_cycle(&mut session) {
            Ok(outcomes) => outcomes,
            Err(failure) => {
                let sample = platform::sample(session.pid()).ok();
                let descendants = sample
                    .as_ref()
                    .map_or_else(Vec::new, |sample| sample.descendants.clone());
                let server_exit_status = session.close();
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
                    "active_descendant_pids": descendants,
                    "server_exit_status": server_exit_status,
                    "finished_unix_ms": SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("system time")
                        .as_millis(),
                }));
                panic!(
                    "{} request {} failed during iteration {}",
                    failure.tool, failure.request_id, iteration
                );
            }
        };
        let sample = platform::sample(session.pid()).expect("sample server resources");
        if !sample.descendants.is_empty() {
            surviving_descendants.insert(iteration, sample.descendants.clone());
        }
        artifact.write(&json!({
            "record_type": "sample",
            "iteration": iteration,
            "phase": if iteration <= warm_up { "warm_up" } else { "measured" },
            "elapsed_ms": started.elapsed().as_millis(),
            "memory_kind": platform::MEMORY_KIND,
            "memory_bytes": sample.resources.memory_bytes,
            "virtual_memory_bytes": sample.resources.virtual_memory_bytes,
            "resource_kind": platform::RESOURCE_KIND,
            "resource_count": sample.resources.resource_count,
            "threads": sample.resources.threads,
            "active_descendant_pids": sample.descendants,
            "outcomes": outcomes,
        }));
        samples.push(sample.resources);
        wait_for_next_burst_epoch(iteration, iterations);
    }

    let server_exit_status = session.close();
    let measured = &samples[warm_up..];
    let resource_growth = sustained_tail_growth(measured, |sample| sample.resource_count);
    let thread_growth = sustained_tail_growth(measured, |sample| sample.threads);
    let passed = server_exit_status.success
        && surviving_descendants.is_empty()
        && !resource_growth
        && !thread_growth;
    artifact.write(&json!({
        "record_type": "result",
        "outcome": if passed { "pass" } else { "fail" },
        "server_exit_status": server_exit_status,
        "surviving_descendants": surviving_descendants,
        "memory": metric_summary(measured, |sample| sample.memory_bytes),
        "resources": metric_summary(measured, |sample| sample.resource_count),
        "threads": metric_summary(measured, |sample| sample.threads),
        "resource_tail_growth_blocking": resource_growth,
        "thread_tail_growth_blocking": thread_growth,
        "threshold_policy": "zero surviving controlled descendants is blocking; resource and thread tail growth require net growth >= 3, slope >= 0.25 per iteration, and increases in at least half of tail transitions; memory growth is observational",
        "finished_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));
    assert!(
        surviving_descendants.is_empty(),
        "controlled descendants survived: {surviving_descendants:?}"
    );
    assert!(!resource_growth, "resource count grew throughout the tail");
    assert!(!thread_growth, "thread count grew throughout the tail");
    assert!(
        server_exit_status.success,
        "server exited with {}",
        server_exit_status.status
    );
    println!("resource soak artifact: {}", artifact.path.display());
}
