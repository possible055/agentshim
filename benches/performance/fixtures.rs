use super::configuration::grep_workload;
use super::tools::benchmark_tools;
use super::{
    Arc, AtomicUsize, BENCH_SCALES_ENV, BENCH_SCOPES_ENV, FileAccess, GrepFileSize, Ordering,
    OsString, Path, ReadScope, RepositoryRoot, fs,
};

pub(super) struct MmapTraceLogger;

static MMAP_TRACE_LOGGER: MmapTraceLogger = MmapTraceLogger;
static MMAP_SELECTED: AtomicUsize = AtomicUsize::new(0);
static MMAP_FALLBACK: AtomicUsize = AtomicUsize::new(0);

impl log::Log for MmapTraceLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Trace
            && metadata.target().starts_with("grep_searcher::searcher")
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        if message.contains("searching via memory map") {
            MMAP_SELECTED.fetch_add(1, Ordering::Relaxed);
        } else if message.contains("searching using generic reader") {
            MMAP_FALLBACK.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn flush(&self) {}
}

pub(super) fn init_mmap_trace() {
    log::set_logger(&MMAP_TRACE_LOGGER).expect("benchmark logger");
    log::set_max_level(log::LevelFilter::Trace);
}

pub(super) fn reset_mmap_trace() {
    MMAP_SELECTED.store(0, Ordering::Relaxed);
    MMAP_FALLBACK.store(0, Ordering::Relaxed);
}

pub(super) fn mmap_trace() -> (usize, usize) {
    (
        MMAP_SELECTED.load(Ordering::Relaxed),
        MMAP_FALLBACK.load(Ordering::Relaxed),
    )
}

pub(super) fn fixture_scales() -> Vec<usize> {
    let configured = std::env::var(BENCH_SCALES_ENV).unwrap_or_else(|_| "1000".to_owned());
    let scales = configured
        .split(',')
        .map(str::trim)
        .map(|value| value.parse::<usize>().expect("fixture scale is an integer"))
        .collect::<Vec<_>>();
    assert!(
        scales.iter().all(|scale| matches!(
            *scale,
            1_000 | 2_000 | 4_000 | 8_000 | 10_000 | 100_000 | 1_000_000
        )),
        "{BENCH_SCALES_ENV} accepts only 1000,2000,4000,8000,10000,100000,1000000"
    );
    scales
}

pub(super) fn benchmark_scale(files: usize) {
    let fixture = tempfile::tempdir().expect("performance fixture");
    let repository_directory = fixture.path().join("repository");
    let repository_corpus = repository_directory.join("corpus");
    let codex_home = fixture.path().join("codex");
    let codex_corpus = codex_home.join("skills").join("corpus");
    create_corpus(&repository_corpus, files);
    create_corpus(&codex_corpus, files);
    let codex_corpus = fs::canonicalize(codex_corpus).expect("canonical Codex corpus");

    let root = Arc::new(RepositoryRoot::open(&repository_directory).expect("repository root"));
    let previous_codex_home = std::env::var_os("CODEX_HOME");
    set_codex_home(Some(codex_home.into_os_string()));
    let normal_access = Arc::new(FileAccess::new(Arc::clone(&root), ReadScope::Normal));
    set_codex_home(previous_codex_home);
    let unrestricted_access = Arc::new(FileAccess::new(Arc::clone(&root), ReadScope::Unrestricted));
    let repository_access = Arc::new(FileAccess::new(Arc::clone(&root), ReadScope::Normal));
    let read_file_name = if matches!(grep_workload().file_size, GrepFileSize::Legacy) {
        "file-000000000.rs"
    } else {
        "file-000000000.selected.rs"
    };
    let repository_read_file = format!("corpus/shard-000000/{read_file_name}");

    if scope_enabled("repository") {
        benchmark_tools(
            "repository",
            files,
            &repository_access,
            "corpus",
            &repository_read_file,
        );
    }
    let codex_path = codex_corpus.to_string_lossy().into_owned();
    let codex_file = codex_corpus
        .join("shard-000000")
        .join(read_file_name)
        .to_string_lossy()
        .into_owned();
    if scope_enabled("normal_codex") {
        benchmark_tools(
            "normal_codex",
            files,
            &normal_access,
            &codex_path,
            &codex_file,
        );
    }
    if scope_enabled("unrestricted") {
        benchmark_tools(
            "unrestricted",
            files,
            &unrestricted_access,
            &codex_path,
            &codex_file,
        );
    }
}

pub(super) fn scope_enabled(scope: &str) -> bool {
    std::env::var(BENCH_SCOPES_ENV).map_or(true, |value| {
        value.split(',').map(str::trim).any(|value| value == scope)
    })
}

pub(super) fn set_codex_home(value: Option<OsString>) {
    match value {
        Some(value) => unsafe { std::env::set_var("CODEX_HOME", value) },
        None => unsafe { std::env::remove_var("CODEX_HOME") },
    }
}

pub(super) fn create_corpus(directory: &Path, files: usize) {
    let workload = grep_workload();
    let selected_files = workload.selected_files(files);
    for index in 0..files {
        let shard = directory.join(format!("shard-{:06}", index / 1_000));
        if index % 1_000 == 0 {
            fs::create_dir_all(&shard).expect("corpus shard");
        }
        let selected = index < selected_files;
        let content = if selected {
            workload.content(index)
        } else {
            format!("pub fn fixture_{index}() {{}}\n")
        };
        let suffix = if selected && !matches!(workload.file_size, GrepFileSize::Legacy) {
            "selected.rs"
        } else {
            "rs"
        };
        fs::write(shard.join(format!("file-{index:09}.{suffix}")), content).expect("corpus file");
    }
}
