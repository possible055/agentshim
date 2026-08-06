use std::{fs, sync::Arc, time::Instant};

use codexshim::{
    path::{FileAccess, ReadScope, RepositoryRoot},
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
    let repository_root = Arc::new(RepositoryRoot::open(current_dir).expect("repository root"));
    let repository_access = Arc::new(FileAccess::new(
        Arc::clone(&repository_root),
        ReadScope::Normal,
    ));
    let unrestricted_access = Arc::new(FileAccess::new(repository_root, ReadScope::Unrestricted));
    let cancellation = CancellationToken::new();
    let worker_lanes = RuntimeConfig::from_env()
        .expect("runtime configuration")
        .worker_lanes;

    benchmark_repository(&repository_access, worker_lanes, &cancellation);
    benchmark_unrestricted(&unrestricted_access, worker_lanes, &cancellation);
}

fn benchmark_repository(
    access: &Arc<FileAccess>,
    worker_lanes: usize,
    cancellation: &CancellationToken,
) {
    measure("glob", || {
        glob::execute(
            access,
            &GlobRequest {
                pattern: "src/**/*.rs".to_owned(),
                path: Some("src".to_owned()),
                include_ignored: None,
                offset: None,
                limit: Some(1_000),
            },
            cancellation,
        )
        .expect("glob benchmark")
    });
    measure("grep", || {
        grep::execute(
            access,
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
            cancellation,
        )
        .expect("grep benchmark")
    });
    measure("read", || {
        read::execute(
            access,
            &ReadRequest {
                path: "README.md".to_owned(),
                start_line: Some(1),
                line_count: Some(1_000),
                encoding: None,
            },
            cancellation,
        )
        .expect("read benchmark")
    });
}

fn benchmark_unrestricted(
    access: &Arc<FileAccess>,
    worker_lanes: usize,
    cancellation: &CancellationToken,
) {
    let ambient_fixture = tempfile::tempdir().expect("ambient fixture");
    let ambient_read_path = ambient_fixture.path().join("small.txt");
    fs::write(&ambient_read_path, "small ambient file\n").expect("ambient read fixture");
    for index in 0..128 {
        fs::write(
            ambient_fixture.path().join(format!("dense-{index:03}.rs")),
            "pub fn ambient_match() {}\n",
        )
        .expect("ambient search fixture");
    }
    let ambient_directory = ambient_fixture.path().display().to_string();
    let ambient_file = ambient_read_path.display().to_string();

    measure("glob_unrestricted", || {
        glob::execute(
            access,
            &GlobRequest {
                pattern: "*.rs".to_owned(),
                path: Some(ambient_directory.clone()),
                include_ignored: None,
                offset: None,
                limit: Some(1_000),
            },
            cancellation,
        )
        .expect("unrestricted glob benchmark")
    });
    measure("grep_unrestricted", || {
        grep::execute(
            access,
            &GrepRequest {
                pattern: "ambient_match".to_owned(),
                path: Some(ambient_directory.clone()),
                glob: Some("*.rs".to_owned()),
                mode: Some(GrepMode::Count),
                fixed_strings: Some(true),
                case: None,
                context_lines: None,
                offset: None,
                limit: Some(1_000),
            },
            worker_lanes,
            cancellation,
        )
        .expect("unrestricted grep benchmark")
    });
    measure("read_unrestricted", || {
        read::execute(
            access,
            &ReadRequest {
                path: ambient_file.clone(),
                start_line: Some(1),
                line_count: Some(1_000),
                encoding: None,
            },
            cancellation,
        )
        .expect("unrestricted read benchmark")
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
