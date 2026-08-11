use super::support::*;
use super::*;

#[test]
#[ignore = "manual pending-request growth probe; run with --ignored --nocapture"]
fn pending_request_growth_probe() {
    const STDIN_BYTES: usize = 1024 * 1024;
    const DEFAULT_REQUESTS: usize = 32;

    let request_count = env::var("CODEXSHIM_PENDING_REQUESTS").map_or(DEFAULT_REQUESTS, |value| {
        value
            .parse::<usize>()
            .expect("CODEXSHIM_PENDING_REQUESTS must be a positive integer")
    });
    assert!(request_count >= 4, "probe requires at least four requests");
    let output = env::var_os("CODEXSHIM_PENDING_OUTPUT").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("resource-soak")
                .join(format!("{}-pending.jsonl", env::consts::OS))
        },
        PathBuf::from,
    );
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).expect("create pending artifact directory");
    }
    let mut artifact = Artifact {
        writer: BufWriter::new(File::create(&output).expect("create pending artifact")),
        path: output,
    };
    artifact.write(&json!({
        "record_type": "metadata",
        "schema_version": 1,
        "scenario": "pending_process_request_growth",
        "platform": env::consts::OS,
        "architecture": env::consts::ARCH,
        "commit": command_output("git", &["rev-parse", "HEAD"]),
        "worktree_status": command_output("git", &["status", "--short"]),
        "toolchain": command_output("cargo", &["--version", "--verbose"]),
        "runner_image": runner_image(),
        "request_count": request_count,
        "encoded_payload_bytes_per_request": STDIN_BYTES,
        "active_process_limit": 16,
        "active_read_only_limit": 16,
        "blocking_thread_limit": 34,
        "admission_mode": "class_aware_fail_fast_16_16",
        "started_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));

    let mut session = Session::start();
    session.discover();
    let executable = env::current_exe().expect("integration test executable");
    let payload = "p".repeat(STDIN_BYTES);
    let mut samples = Vec::new();
    let baseline = platform::sample(session.pid()).expect("sample pending baseline");
    artifact.write(&pending_sample(0, &baseline));
    samples.push((0_usize, baseline.resources.memory_bytes));

    let checkpoints = [
        request_count / 4,
        request_count / 2,
        request_count.saturating_mul(3) / 4,
        request_count,
    ];
    let mut sent = 0_usize;
    for checkpoint in checkpoints {
        while sent < checkpoint {
            session.send_tool(
                "run_program",
                json!({
                    "program": executable,
                    "args": ["--exact", "aggregate::pending_process_child_fixture", "--nocapture"],
                    "cwd": env!("CARGO_MANIFEST_DIR"),
                    "env": { "CODEXSHIM_PENDING_FIXTURE": "child" },
                    "stdin": payload,
                    "timeout_ms": 30_000,
                }),
            );
            sent += 1;
        }
        thread::sleep(Duration::from_millis(300));
        let sample = platform::sample(session.pid()).expect("sample pending requests");
        artifact.write(&pending_sample(sent, &sample));
        samples.push((sent, sample.resources.memory_bytes));
    }

    let initial_memory = samples.first().expect("initial sample").1;
    let final_memory = samples.last().expect("final sample").1;
    let slope = request_slope(&samples);
    artifact.write(&json!({
        "record_type": "result",
        "outcome": "measured",
        "initial_memory_bytes": initial_memory,
        "final_memory_bytes": final_memory,
        "retained_growth_bytes": final_memory.saturating_sub(initial_memory),
        "least_squares_bytes_per_queued_request": slope,
        "threshold_policy": "adoption evidence only; not an absolute RSS regression gate",
        "finished_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));
    session.stdin.take();
    session.close();
    println!("pending request artifact: {}", artifact.path.display());
}

fn pending_sample(request_count: usize, sample: &Sample) -> Value {
    json!({
        "record_type": "sample",
        "queued_request_count": request_count,
        "memory_kind": platform::MEMORY_KIND,
        "memory_bytes": sample.resources.memory_bytes,
        "virtual_memory_bytes": sample.resources.virtual_memory_bytes,
        "resource_kind": platform::RESOURCE_KIND,
        "resource_count": sample.resources.resource_count,
        "threads": sample.resources.threads,
        "active_descendant_pids": sample.descendants,
    })
}

pub(super) fn request_slope(samples: &[(usize, u64)]) -> f64 {
    let count = f64::from(u32::try_from(samples.len()).expect("bounded sample count"));
    let mean_x = samples
        .iter()
        .map(|(requests, _)| metric_as_f64(u64::try_from(*requests).expect("request count")))
        .sum::<f64>()
        / count;
    let mean_y = samples
        .iter()
        .map(|(_, memory)| metric_as_f64(*memory))
        .sum::<f64>()
        / count;
    let (numerator, denominator) = samples.iter().fold(
        (0.0, 0.0),
        |(numerator, denominator), (requests, memory)| {
            let x = metric_as_f64(u64::try_from(*requests).expect("request count")) - mean_x;
            (
                numerator + x * (metric_as_f64(*memory) - mean_y),
                denominator + x * x,
            )
        },
    );
    if denominator == 0.0 {
        0.0
    } else {
        numerator / denominator
    }
}
