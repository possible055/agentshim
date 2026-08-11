use super::support::*;
use super::*;

#[test]
#[ignore = "manual resource soak; run with --ignored --nocapture"]
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
        "sample_unit": "one sequential read, glob, grep, and run_program cycle",
        "started_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));

    let mut session = Session::start();
    session.discover();
    let mut samples = Vec::with_capacity(iterations);
    let mut surviving_descendants = BTreeMap::<usize, Vec<u32>>::new();
    for iteration in 1..=iterations {
        let started = Instant::now();
        let outcomes = run_mixed_cycle(&mut session);
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
    }

    let server_exit_status = session.close();
    let measured = &samples[warm_up..];
    artifact.write(&json!({
        "record_type": "result",
        "outcome": if surviving_descendants.is_empty() { "pass" } else { "fail" },
        "server_exit_status": server_exit_status,
        "surviving_descendants": surviving_descendants,
        "memory": metric_summary(measured, |sample| sample.memory_bytes),
        "resources": metric_summary(measured, |sample| sample.resource_count),
        "threads": metric_summary(measured, |sample| sample.threads),
        "threshold_policy": "observational growth; zero surviving controlled descendants is blocking",
        "finished_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));
    assert!(
        surviving_descendants.is_empty(),
        "controlled descendants survived: {surviving_descendants:?}"
    );
    println!("resource soak artifact: {}", artifact.path.display());
}
