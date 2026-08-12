use super::reporting::quick_mode;
use super::*;

pub(super) fn grep_lanes() -> &'static [usize] {
    static CONFIGURED: OnceLock<Option<Vec<usize>>> = OnceLock::new();
    if let Some(configured) = CONFIGURED
        .get_or_init(|| {
            std::env::var(GREP_LANES_ENV).ok().map(|value| {
                let lanes = value
                    .split(',')
                    .map(str::trim)
                    .map(|value| value.parse::<usize>().expect("grep lane is an integer"))
                    .collect::<Vec<_>>();
                assert!(
                    !lanes.is_empty() && lanes.iter().all(|lane| GREP_LANES.contains(lane)),
                    "{GREP_LANES_ENV} accepts only comma-separated values from 1,2,4,8,16"
                );
                lanes
            })
        })
        .as_deref()
    {
        return configured;
    }
    if quick_mode() {
        &GREP_LANES[4..]
    } else {
        &GREP_LANES
    }
}

pub(super) fn grep_mode() -> GrepMode {
    match std::env::var(GREP_MODE_ENV).as_deref() {
        Ok("content") | Err(std::env::VarError::NotPresent) => GrepMode::Content,
        Ok("files") => GrepMode::Files,
        Ok("count") => GrepMode::Count,
        Ok(value) => panic!("{GREP_MODE_ENV} accepts only content,files,count; got {value}"),
        Err(error) => panic!("{GREP_MODE_ENV} is not valid Unicode: {error}"),
    }
}

pub(super) fn grep_limit() -> usize {
    std::env::var(GREP_LIMIT_ENV).map_or(1_000, |value| {
        value
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=1_000).contains(limit))
            .unwrap_or_else(|| panic!("{GREP_LIMIT_ENV} must be from 1 to 1000"))
    })
}

pub(super) fn grep_offset() -> usize {
    std::env::var(GREP_OFFSET_ENV).map_or(0, |value| {
        value
            .parse::<usize>()
            .unwrap_or_else(|_| panic!("{GREP_OFFSET_ENV} must be a non-negative integer"))
    })
}

pub(super) fn grep_sources() -> &'static [(&'static str, grep::GrepSourcePolicy)] {
    const DEFAULT: [(&str, grep::GrepSourcePolicy); 1] =
        [("hybrid", grep::GrepSourcePolicy::Hybrid)];
    static CONFIGURED: OnceLock<Option<Vec<(&'static str, grep::GrepSourcePolicy)>>> =
        OnceLock::new();
    CONFIGURED
        .get_or_init(|| {
            std::env::var(GREP_SOURCES_ENV).ok().map(|value| {
                let sources = value
                    .split(',')
                    .map(str::trim)
                    .map(|value| match value {
                        "hybrid" => ("hybrid", grep::GrepSourcePolicy::Hybrid),
                        value if value.starts_with("capture-limit:") => {
                            let bytes = value["capture-limit:".len()..]
                                .parse::<u64>()
                                .ok()
                                .filter(|bytes| *bytes > 0)
                                .expect("capture limit is a positive byte count");
                            let name = Box::leak(value.to_owned().into_boxed_str());
                            (&*name, grep::GrepSourcePolicy::CaptureLimit(bytes))
                        }
                        "reader" => ("reader", grep::GrepSourcePolicy::Reader),
                        "file-never" => ("file-never", grep::GrepSourcePolicy::FileNever),
                        "mmap-always" => ("mmap-always", grep::GrepSourcePolicy::MmapAlways),
                        value if value.starts_with("mmap-threshold:") => {
                            let bytes = value["mmap-threshold:".len()..]
                                .parse::<u64>()
                                .ok()
                                .filter(|bytes| *bytes > 0)
                                .expect("mmap threshold is a positive byte count");
                            let name = Box::leak(value.to_owned().into_boxed_str());
                            (&*name, grep::GrepSourcePolicy::MmapThreshold(bytes))
                        }
                        _ => panic!(
                            "{GREP_SOURCES_ENV} accepts hybrid,capture-limit:<bytes>,reader,\
                             file-never,mmap-always,or mmap-threshold:<bytes>"
                        ),
                    })
                    .collect::<Vec<_>>();
                assert!(!sources.is_empty(), "{GREP_SOURCES_ENV} is empty");
                sources
            })
        })
        .as_deref()
        .unwrap_or(&DEFAULT)
}

pub(super) fn pathname_reopen_variants() -> &'static [(&'static str, grep::PathnameReopenPolicy)] {
    const DEFAULT: [(&str, grep::PathnameReopenPolicy); 1] =
        [("off", grep::PathnameReopenPolicy::Off)];
    static CONFIGURED: OnceLock<Option<Vec<(&'static str, grep::PathnameReopenPolicy)>>> =
        OnceLock::new();
    CONFIGURED
        .get_or_init(|| {
            std::env::var(GREP_PATHNAME_REOPEN_ENV).ok().map(|value| {
                let variants = value
                    .split(',')
                    .map(str::trim)
                    .map(|value| match value {
                        "on" => ("on", grep::PathnameReopenPolicy::On),
                        "off" => ("off", grep::PathnameReopenPolicy::Off),
                        "parent-batch" => ("parent-batch", grep::PathnameReopenPolicy::ParentBatch),
                        _ => {
                            panic!("{GREP_PATHNAME_REOPEN_ENV} accepts only on,off,parent-batch")
                        }
                    })
                    .collect::<Vec<_>>();
                assert!(!variants.is_empty(), "{GREP_PATHNAME_REOPEN_ENV} is empty");
                variants
            })
        })
        .as_deref()
        .unwrap_or(&DEFAULT)
}

pub(super) fn grep_traversals() -> &'static [(&'static str, grep::GrepTraversal)] {
    const DEFAULT: [(&str, grep::GrepTraversal); 3] = [
        ("serial", grep::GrepTraversal::Serial),
        ("parallel_256", grep::GrepTraversal::ParallelBatched),
        ("adaptive", grep::GrepTraversal::Adaptive),
    ];
    static CONFIGURED: OnceLock<Option<Vec<(&'static str, grep::GrepTraversal)>>> = OnceLock::new();
    CONFIGURED
        .get_or_init(|| {
            std::env::var(GREP_TRAVERSALS_ENV).ok().map(|value| {
                let traversals = value
                    .split(',')
                    .map(str::trim)
                    .map(|value| match value {
                        "serial" => ("serial", grep::GrepTraversal::Serial),
                        "parallel_256" => ("parallel_256", grep::GrepTraversal::ParallelBatched),
                        "adaptive" => ("adaptive", grep::GrepTraversal::Adaptive),
                        "serial_prefix" => {
                            ("serial_prefix", grep::GrepTraversal::SerialLiteralPrefix)
                        }
                        "parallel_256_prefix" => (
                            "parallel_256_prefix",
                            grep::GrepTraversal::ParallelBatchedLiteralPrefix,
                        ),
                        _ => panic!(
                            "{GREP_TRAVERSALS_ENV} accepts only serial,parallel_256,adaptive,\
                             serial_prefix,parallel_256_prefix"
                        ),
                    })
                    .collect::<Vec<_>>();
                assert!(!traversals.is_empty(), "{GREP_TRAVERSALS_ENV} is empty");
                traversals
            })
        })
        .as_deref()
        .unwrap_or(&DEFAULT)
}

pub(super) fn grep_workload() -> GrepWorkload {
    static WORKLOAD: OnceLock<GrepWorkload> = OnceLock::new();
    *WORKLOAD.get_or_init(|| {
        let value = std::env::var(GREP_WORKLOAD_ENV).unwrap_or_else(|_| "legacy".to_owned());
        match value.as_str() {
            "legacy" => GrepWorkload {
                name: "legacy",
                file_size: GrepFileSize::Legacy,
                density: MatchDensity::Legacy,
            },
            "1k-none" => GrepWorkload::matrix("1k-none", GrepFileSize::OneKiB, MatchDensity::None),
            "1k-rare" => GrepWorkload::matrix("1k-rare", GrepFileSize::OneKiB, MatchDensity::Rare),
            "1k-dense" => {
                GrepWorkload::matrix("1k-dense", GrepFileSize::OneKiB, MatchDensity::Dense)
            }
            "4k-rare" => GrepWorkload::matrix("4k-rare", GrepFileSize::FourKiB, MatchDensity::Rare),
            "16k-rare" => {
                GrepWorkload::matrix("16k-rare", GrepFileSize::SixteenKiB, MatchDensity::Rare)
            }
            "32k-rare" => {
                GrepWorkload::matrix("32k-rare", GrepFileSize::ThirtyTwoKiB, MatchDensity::Rare)
            }
            "64k-none" => {
                GrepWorkload::matrix("64k-none", GrepFileSize::SixtyFourKiB, MatchDensity::None)
            }
            "64k-rare" => {
                GrepWorkload::matrix("64k-rare", GrepFileSize::SixtyFourKiB, MatchDensity::Rare)
            }
            "64k-dense" => {
                GrepWorkload::matrix("64k-dense", GrepFileSize::SixtyFourKiB, MatchDensity::Dense)
            }
            "256k-rare" => GrepWorkload::matrix(
                "256k-rare",
                GrepFileSize::TwoHundredFiftySixKiB,
                MatchDensity::Rare,
            ),
            "4m-none" => GrepWorkload::matrix("4m-none", GrepFileSize::FourMiB, MatchDensity::None),
            "4m-rare" => GrepWorkload::matrix("4m-rare", GrepFileSize::FourMiB, MatchDensity::Rare),
            "4m-dense" => {
                GrepWorkload::matrix("4m-dense", GrepFileSize::FourMiB, MatchDensity::Dense)
            }
            _ => panic!(
                "{GREP_WORKLOAD_ENV} accepts legacy, 1k-none, 1k-rare, 1k-dense, \
                 4k-rare, 16k-rare, 32k-rare, 64k-none, 64k-rare, 64k-dense, \
                 256k-rare, 4m-none, 4m-rare, or 4m-dense"
            ),
        }
    })
}

impl GrepWorkload {
    const fn matrix(name: &'static str, file_size: GrepFileSize, density: MatchDensity) -> Self {
        Self {
            name,
            file_size,
            density,
        }
    }

    pub(super) fn selected_files(self, files: usize) -> usize {
        if matches!(self.file_size, GrepFileSize::Legacy) {
            return files;
        }
        let configured = std::env::var(GREP_SELECTED_FILES_ENV).unwrap_or_else(|_| {
            panic!("{GREP_SELECTED_FILES_ENV} is required for non-legacy grep workloads")
        });
        if configured == "all" {
            return files;
        }
        configured
            .parse::<usize>()
            .ok()
            .filter(|selected| (1..=files).contains(selected))
            .unwrap_or_else(|| {
                panic!("{GREP_SELECTED_FILES_ENV} must be all or an integer from 1 to {files}")
            })
    }

    pub(super) fn glob(self) -> &'static str {
        match self.file_size {
            GrepFileSize::Legacy => "**/*.rs",
            GrepFileSize::OneKiB
            | GrepFileSize::FourKiB
            | GrepFileSize::SixteenKiB
            | GrepFileSize::ThirtyTwoKiB
            | GrepFileSize::SixtyFourKiB
            | GrepFileSize::TwoHundredFiftySixKiB
            | GrepFileSize::FourMiB => "**/*.selected.rs",
        }
    }

    pub(super) fn content(self, index: usize) -> String {
        if matches!(self.file_size, GrepFileSize::Legacy) {
            return format!("pub fn fixture_{index}() {{}}\nneedle-{index}\n");
        }
        let target = match self.file_size {
            GrepFileSize::Legacy => unreachable!(),
            GrepFileSize::OneKiB => 1_024,
            GrepFileSize::FourKiB => 4 * 1_024,
            GrepFileSize::SixteenKiB => 16 * 1_024,
            GrepFileSize::ThirtyTwoKiB => 32 * 1_024,
            GrepFileSize::SixtyFourKiB => 64 * 1_024,
            GrepFileSize::TwoHundredFiftySixKiB => 256 * 1_024,
            GrepFileSize::FourMiB => 4 * 1024 * 1024,
        };
        let matching = match self.density {
            MatchDensity::Legacy => unreachable!(),
            MatchDensity::None => false,
            MatchDensity::Rare => index.is_multiple_of(100),
            MatchDensity::Dense => true,
        };
        let line = if matches!(self.density, MatchDensity::Dense) {
            format!("needle-{index}\n")
        } else {
            format!("ordinary-{index}\n")
        };
        let mut content = String::with_capacity(target);
        if matching && !matches!(self.density, MatchDensity::Dense) {
            writeln!(content, "needle-{index}").expect("write fixture match");
        }
        while content.len().saturating_add(line.len()) <= target {
            content.push_str(&line);
        }
        content.extend(std::iter::repeat_n(
            'x',
            target.saturating_sub(content.len()),
        ));
        content
    }
}

pub(super) fn concurrency_levels() -> &'static [usize] {
    static CONFIGURED: OnceLock<Option<Vec<usize>>> = OnceLock::new();
    if let Some(configured) = CONFIGURED
        .get_or_init(|| {
            std::env::var(BENCH_CONCURRENCY_LEVELS_ENV)
                .ok()
                .map(|value| {
                    let levels = value
                        .split(',')
                        .map(str::trim)
                        .map(|value| {
                            value
                                .parse::<usize>()
                                .expect("concurrency level is an integer")
                        })
                        .collect::<Vec<_>>();
                    assert!(
                        !levels.is_empty()
                            && levels
                                .iter()
                                .all(|level| CONCURRENCY_LEVELS.contains(level)),
                        "{BENCH_CONCURRENCY_LEVELS_ENV} accepts only comma-separated values from \
                         1,8,16"
                    );
                    levels
                })
        })
        .as_deref()
    {
        return configured;
    }
    if quick_mode() && std::env::var_os(BENCH_QUICK_CONCURRENT_ENV).is_none_or(|value| value != "1")
    {
        &CONCURRENCY_LEVELS[..1]
    } else {
        &CONCURRENCY_LEVELS
    }
}

pub(super) fn percentile(samples: &[f64], numerator: usize, denominator: usize) -> f64 {
    let index = samples
        .len()
        .saturating_mul(numerator)
        .div_ceil(denominator)
        .saturating_sub(1)
        .min(samples.len() - 1);
    samples[index]
}
