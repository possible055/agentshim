use std::{
    ffi::OsString,
    fmt::Write as _,
    fs,
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use codexshim::bench_support::{
    FileAccess, OpenReadStrategy, ReadScope, RepositoryRoot, ResolvedPath,
    glob::{self, GlobRequest, GlobTraversal},
    grep::{self, GrepMode, GrepRequest},
    open_read_batches,
    read::{self, ReadRequest},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const WARM_SAMPLES: usize = 7;
const CONCURRENCY_LEVELS: [usize; 3] = [1, 8, 16];
const GREP_LANES: [usize; 5] = [1, 2, 4, 8, 16];
const BENCH_SCALES_ENV: &str = "CODEXSHIM_BENCH_SCALES";
const BENCH_SCOPES_ENV: &str = "CODEXSHIM_BENCH_SCOPES";
const BENCH_QUICK_ENV: &str = "CODEXSHIM_BENCH_QUICK";
const BENCH_QUICK_CONCURRENT_ENV: &str = "CODEXSHIM_BENCH_QUICK_CONCURRENT";
const BENCH_WARM_SAMPLES_ENV: &str = "CODEXSHIM_BENCH_WARM_SAMPLES";
const BENCH_CONCURRENCY_LEVELS_ENV: &str = "CODEXSHIM_BENCH_CONCURRENCY_LEVELS";
const OPEN_BATCH_ONLY_ENV: &str = "CODEXSHIM_BENCH_OPEN_BATCH_ONLY";
const GREP_LANES_ENV: &str = "CODEXSHIM_BENCH_GREP_LANES";
const GREP_TRAVERSALS_ENV: &str = "CODEXSHIM_BENCH_GREP_TRAVERSALS";
const GREP_WORKLOAD_ENV: &str = "CODEXSHIM_BENCH_GREP_WORKLOAD";
const GREP_GLOB_ENV: &str = "CODEXSHIM_BENCH_GREP_GLOB";
const GREP_SELECTED_FILES_ENV: &str = "CODEXSHIM_BENCH_GREP_SELECTED_FILES";
const GLOB_VARIANTS_ONLY_ENV: &str = "CODEXSHIM_BENCH_GLOB_VARIANTS_ONLY";
const GLOB_PROFILE_ONLY_ENV: &str = "CODEXSHIM_BENCH_GLOB_PROFILE_ONLY";
const GLOB_PATTERN_ENV: &str = "CODEXSHIM_BENCH_GLOB_PATTERN";
const GLOB_TRAVERSALS_ENV: &str = "CODEXSHIM_BENCH_GLOB_TRAVERSALS";
const GREP_VARIANTS_ONLY_ENV: &str = "CODEXSHIM_BENCH_GREP_VARIANTS_ONLY";
const GREP_PROFILE_ENV: &str = "CODEXSHIM_BENCH_PROFILE_GREP";
const GREP_PROFILE_ONLY_ENV: &str = "CODEXSHIM_BENCH_GREP_PROFILE_ONLY";
const GREP_COMPACT_PROFILE_ENV: &str = "CODEXSHIM_BENCH_GREP_COMPACT_PROFILE";
const GREP_SOURCES_ENV: &str = "CODEXSHIM_BENCH_GREP_SOURCES";
const GREP_PATHNAME_REOPEN_ENV: &str = "CODEXSHIM_BENCH_GREP_PATHNAME_REOPEN";
const GREP_MODE_ENV: &str = "CODEXSHIM_BENCH_GREP_MODE";
const READ_START_LINE_ENV: &str = "CODEXSHIM_BENCH_READ_START_LINE";
const READ_LINE_COUNT_ENV: &str = "CODEXSHIM_BENCH_READ_LINE_COUNT";
const READ_ONLY_ENV: &str = "CODEXSHIM_BENCH_READ_ONLY";
const BENCH_COMMIT_ENV: &str = "CODEXSHIM_BENCH_COMMIT";
const BENCH_WORKTREE_ENV: &str = "CODEXSHIM_BENCH_WORKTREE";
const GLOB_P95_ENV: &str = "CODEXSHIM_BENCH_MAX_GLOB_P95_MS_PER_1K";
const GREP_P95_ENV: &str = "CODEXSHIM_BENCH_MAX_GREP_P95_MS_PER_1K";
const READ_P95_ENV: &str = "CODEXSHIM_BENCH_MAX_READ_P95_MS";

#[derive(Clone, Copy)]
enum GrepFileSize {
    Legacy,
    OneKiB,
    FourKiB,
    SixteenKiB,
    ThirtyTwoKiB,
    SixtyFourKiB,
    TwoHundredFiftySixKiB,
    FourMiB,
}

#[derive(Clone, Copy)]
enum MatchDensity {
    Legacy,
    None,
    Rare,
    Dense,
}

#[derive(Clone, Copy)]
struct GrepWorkload {
    name: &'static str,
    file_size: GrepFileSize,
    density: MatchDensity,
}

#[path = "performance/configuration.rs"]
mod configuration;
#[path = "performance/fixtures.rs"]
mod fixtures;
#[path = "performance/reporting.rs"]
mod reporting;
#[path = "performance/tools.rs"]
mod tools;

fn main() {
    fixtures::init_mmap_trace();
    for files in fixtures::fixture_scales() {
        fixtures::benchmark_scale(files);
        println!(
            "{}",
            json!({ "benchmark": "scale_complete", "fixture_files": files })
        );
    }
    std::process::exit(0);
}
