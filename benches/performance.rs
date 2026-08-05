use std::{sync::Arc, time::Instant};

use codexshim::{
    path::RepositoryRoot,
    runtime::RuntimeConfig,
    tools::{
        glob::{self, GlobRequest},
        grep::{self, GrepMode, GrepRequest},
        read::{self, ReadRequest},
    },
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const SAMPLES: usize = 5;

fn main() {
    let current_dir = std::env::current_dir().expect("current directory");
    let root = Arc::new(RepositoryRoot::open(current_dir).expect("repository root"));
    let cancellation = CancellationToken::new();
    let worker_lanes = RuntimeConfig::from_env()
        .expect("runtime configuration")
        .worker_lanes;

    measure("glob", || {
        glob::execute(
            &root,
            &GlobRequest {
                pattern: "src/**/*.rs".to_owned(),
                path: Some("src".to_owned()),
                include_ignored: None,
                offset: None,
                limit: Some(1_000),
            },
            &cancellation,
        )
        .expect("glob benchmark")
    });
    measure("grep", || {
        grep::execute(
            &root,
            &GrepRequest {
                pattern: "pub".to_owned(),
                path: Some("src".to_owned()),
                glob: Some("src/**/*.rs".to_owned()),
                mode: Some(GrepMode::Count),
                fixed_strings: Some(true),
                case: None,
                context_lines: None,
                offset: None,
                limit: Some(1_000),
            },
            worker_lanes,
            &cancellation,
        )
        .expect("grep benchmark")
    });
    measure("read", || {
        read::execute(
            &root,
            &ReadRequest {
                path: "README.md".to_owned(),
                start_line: Some(1),
                line_count: Some(1_000),
                encoding: None,
            },
            &cancellation,
        )
        .expect("read benchmark")
    });
}

fn measure(operation: &str, mut execute: impl FnMut() -> String) {
    assert!(
        !execute().is_empty(),
        "{operation} benchmark returned no output"
    );
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        let output = execute();
        assert!(
            !output.is_empty(),
            "{operation} benchmark returned no output"
        );
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    println!(
        "{}",
        json!({ "operation": operation, "samples_ms": samples })
    );
}
