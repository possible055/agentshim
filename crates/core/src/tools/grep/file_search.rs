use std::{
    collections::BTreeMap,
    io::{self, Read, Seek, SeekFrom},
    sync::Arc,
};

use cap_std::fs::File;
use grep_matcher::{LineTerminator, Matcher};
use grep_regex::RegexMatcher;
use grep_searcher::{
    BinaryDetection, MmapChoice, Searcher, SearcherBuilder, Sink, SinkContext, SinkError, SinkMatch,
};
use tokio_util::sync::CancellationToken;

use crate::{
    output::SkipReason,
    path::{FileAccess, ResolvedPath},
    runtime::RuntimeResources,
    tools::read::FileFingerprint,
};

use super::{
    profile::{GrepProfiler, GrepStage},
    request::{
        CONTENT_OMISSION, GrepBenchmarkVariant, GrepError, GrepMode, GrepSourcePolicy,
        PathnameReopenPolicy, SEARCH_HEAP_BYTES,
    },
};

use super::candidates::Candidate;
#[derive(Clone, Copy)]
pub struct SearchPlan {
    pub mode: GrepMode,
    pub context: usize,
    pub probe: usize,
    pub skip: usize,
    pub allow_early_stop: bool,
    /// Forces the decoding of a single-file target. Validated to a canonical label before
    /// the search starts, so no candidate pays for label resolution.
    pub encoding: Option<&'static str>,
    /// Applies only where detection cannot name an encoding on its own. It never displaces
    /// a BOM, valid UTF-8, or a detected legacy encoding, because forcing one label across
    /// a whole directory would corrupt the files that were already readable.
    pub fallback_encoding: Option<&'static str>,
    pub memory: super::request::GrepMemoryPolicy,
}

/// How one candidate's bytes must be treated before the matcher sees them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceHandling {
    /// Bytes are searched as they are, through the existing fast paths.
    Native,
    /// Bytes are legacy-encoded text and must be decoded before searching.
    Transcode(&'static str),
    /// Nothing can read this file as text.
    Skip(SkipReason),
}

pub struct FileSearchContext<'a> {
    pub access: &'a FileAccess,
    pub matcher: &'a RegexMatcher,
    pub plan: SearchPlan,
    pub cancellation: &'a CancellationToken,
    pub retirement: &'a CancellationToken,
    pub variant: GrepBenchmarkVariant,
    pub profiler: &'a GrepProfiler,
    pub resources: Option<&'a RuntimeResources>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RecordKind {
    Context,
    Match,
}

#[derive(Debug)]
pub struct Record {
    pub line: u64,
    order: usize,
    pub kind: RecordKind,
    pub text: String,
}

#[derive(Debug)]
pub struct FileOutcome {
    pub path: Option<Arc<ResolvedPath>>,
    pub records: Vec<Record>,
    pub entries: usize,
    pub occurrences: usize,
    pub matched: bool,
    pub skip: Option<SkipReason>,
    pub retired: bool,
    pub leading_skipped: usize,
    pub retry: Option<RetryReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryReason {
    Capture,
    HeapLimit,
}

#[cfg(test)]
pub fn search_file(
    access: &FileAccess,
    candidate: &Candidate,
    matcher: &RegexMatcher,
    plan: SearchPlan,
    cancellation: &CancellationToken,
) -> Result<FileOutcome, GrepError> {
    let variant = GrepBenchmarkVariant::default();
    let mut searcher = build_searcher(plan, variant.source);
    let profiler = GrepProfiler::disabled();
    let retirement = CancellationToken::new();
    let context = FileSearchContext {
        access,
        matcher,
        plan,
        cancellation,
        retirement: &retirement,
        variant,
        profiler: &profiler,
        resources: None,
    };
    search_file_with_searcher(candidate, &context, &mut searcher, || {})
}

#[cfg(test)]
pub fn search_file_with_hook(
    access: &FileAccess,
    candidate: &Candidate,
    matcher: &RegexMatcher,
    plan: SearchPlan,
    cancellation: &CancellationToken,
    after_search: impl FnOnce(),
) -> Result<FileOutcome, GrepError> {
    let variant = GrepBenchmarkVariant::default();
    let mut searcher = build_searcher(plan, variant.source);
    let profiler = GrepProfiler::disabled();
    let retirement = CancellationToken::new();
    let context = FileSearchContext {
        access,
        matcher,
        plan,
        cancellation,
        retirement: &retirement,
        variant,
        profiler: &profiler,
        resources: None,
    };
    search_file_with_searcher(candidate, &context, &mut searcher, after_search)
}

#[cfg(test)]
pub fn search_file_with_variant_hook(
    access: &FileAccess,
    candidate: &Candidate,
    matcher: &RegexMatcher,
    plan: SearchPlan,
    cancellation: &CancellationToken,
    variant: GrepBenchmarkVariant,
    after_search: impl FnOnce(),
) -> Result<FileOutcome, GrepError> {
    let mut searcher = build_searcher(plan, variant.source);
    let profiler = GrepProfiler::disabled();
    let retirement = CancellationToken::new();
    let context = FileSearchContext {
        access,
        matcher,
        plan,
        cancellation,
        retirement: &retirement,
        variant,
        profiler: &profiler,
        resources: None,
    };
    search_file_with_searcher(candidate, &context, &mut searcher, after_search)
}

pub fn build_searcher(plan: SearchPlan, source: GrepSourcePolicy) -> Searcher {
    let mut builder = SearcherBuilder::new();
    let mmap = match source {
        GrepSourcePolicy::Hybrid => MmapChoice::never(),
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::CaptureLimit(_) | GrepSourcePolicy::Reader => MmapChoice::never(),
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::FileNever => MmapChoice::never(),
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::MmapAlways | GrepSourcePolicy::MmapThreshold(_) => {
            // SAFETY: These variants are benchmark-only entry points. Mutation coverage runs in
            // a subprocess because external mutation can invalidate a file-backed mapping.
            unsafe { MmapChoice::auto() }
        }
    };
    builder
        .line_number(true)
        .line_terminator(LineTerminator::crlf())
        .before_context(plan.context)
        .after_context(plan.context)
        .heap_limit(Some(plan.memory.base_search_heap_bytes))
        .memory_map(mmap)
        .bom_sniffing(true)
        .binary_detection(BinaryDetection::quit(0));
    if plan.mode == GrepMode::Files && plan.allow_early_stop {
        builder.max_matches(Some(1));
    }
    builder.build()
}

pub fn search_file_with_searcher(
    candidate: &Candidate,
    context: &FileSearchContext<'_>,
    searcher: &mut Searcher,
    after_search: impl FnOnce(),
) -> Result<FileOutcome, GrepError> {
    if context.cancellation.is_cancelled() {
        return Err(GrepError::Cancelled);
    }
    let open_span = context.profiler.span(GrepStage::SearchOpenWorker);
    let opened = open_candidate(
        context.access,
        &candidate.path,
        context.profiler,
        GrepStage::SearchBeforeFingerprintWorker,
        requires_path_identity(context.variant.pathname_reopen),
    );
    drop(open_span);
    let Ok(OpenedCandidate {
        file,
        fingerprint: before,
    }) = opened
    else {
        return Ok(FileOutcome::skipped(
            Some(Arc::clone(&candidate.path)),
            SkipReason::Io,
        ));
    };
    search_opened_candidate_with_searcher(candidate, context, searcher, after_search, file, &before)
}

pub fn search_opened_candidate_with_searcher(
    candidate: &Candidate,
    context: &FileSearchContext<'_>,
    searcher: &mut Searcher,
    after_search: impl FnOnce(),
    file: File,
    before: &FileFingerprint,
) -> Result<FileOutcome, GrepError> {
    let mut file = file;
    let handling = match classify_source(&mut file, context.plan, context.cancellation) {
        Ok(handling) => handling,
        Err(SearchError::Cancelled) => return Err(GrepError::Cancelled),
        Err(_) => {
            return Ok(FileOutcome::skipped(
                Some(Arc::clone(&candidate.path)),
                SkipReason::Io,
            ));
        }
    };
    if let SourceHandling::Skip(reason) = handling {
        return Ok(FileOutcome::skipped(
            Some(Arc::clone(&candidate.path)),
            reason,
        ));
    }
    let mut sink = PlanSink::new(
        context.matcher,
        context.plan,
        context.cancellation,
        context.retirement,
    );
    context.profiler.record_searched_candidate();
    let scan_span = context.profiler.span(GrepStage::SearchScanWorker);
    let (search_result, file) = match handling {
        SourceHandling::Transcode(label) => {
            search_transcoded(file, label, context, searcher, &mut sink)
        }
        SourceHandling::Native | SourceHandling::Skip(_) => {
            search_source(file, before, context, searcher, &mut sink)
        }
    };
    drop(scan_span);
    if let Err(error) = search_result {
        return outcome_for_search_error(error, candidate, sink);
    }
    after_search();
    let verify_span = context.profiler.span(GrepStage::SearchVerifyWorker);
    let after_span = context
        .profiler
        .span(GrepStage::SearchAfterFingerprintWorker);
    let unchanged = before.matches_current_state(&file)?;
    drop(after_span);
    if !unchanged {
        return Ok(FileOutcome::skipped(
            Some(Arc::clone(&candidate.path)),
            SkipReason::ChangedWhileSearched,
        ));
    }
    #[cfg(any(test, feature = "bench-internals"))]
    if context.variant.pathname_reopen == PathnameReopenPolicy::On {
        context.profiler.record_pathname_reopen();
        let reopen_span = context.profiler.span(GrepStage::SearchPathnameReopenWorker);
        let identity = match open_identity_candidate(
            context.access,
            &candidate.path,
            context.profiler,
            GrepStage::SearchPathnameFingerprintWorker,
        ) {
            Ok(identity) => identity.fingerprint,
            Err(_) => {
                return Ok(FileOutcome::skipped(
                    Some(Arc::clone(&candidate.path)),
                    SkipReason::Io,
                ));
            }
        };
        drop(reopen_span);
        if *before != identity {
            return Ok(FileOutcome::skipped(
                Some(Arc::clone(&candidate.path)),
                SkipReason::ChangedWhileSearched,
            ));
        }
    }
    drop(verify_span);
    let path = Some(Arc::clone(&candidate.path));
    let outcome = sink.finish(path)?;
    if outcome.matched {
        context.profiler.record_matched_candidate();
    }
    Ok(outcome)
}

fn search_source(
    file: File,
    before: &FileFingerprint,
    context: &FileSearchContext<'_>,
    searcher: &mut Searcher,
    sink: &mut PlanSink<'_>,
) -> (Result<(), SearchError>, File) {
    let memory_source_limit = memory_source_limit(context.variant.source, context.plan.memory);
    let use_memory_source = context.plan.mode == GrepMode::Content
        && memory_source_limit > 0
        && before.length() <= memory_source_limit;
    let use_search_file = match context.variant.source {
        GrepSourcePolicy::Hybrid => true,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::CaptureLimit(_) => true,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::Reader => false,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::FileNever | GrepSourcePolicy::MmapAlways => true,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::MmapThreshold(minimum_bytes) => before.length() >= minimum_bytes,
    };
    let memory_bytes = if use_memory_source {
        usize::try_from(before.length()).ok().and_then(|len| {
            context
                .resources?
                .try_reserve_memory(len)
                .map(|permit| (len, permit))
        })
    } else {
        None
    };
    if let Some((len, memory_permit)) = memory_bytes {
        let capture_span = context.profiler.span(GrepStage::CaptureReadWorker);
        let mut file = file;
        let mut bytes = Vec::with_capacity(len);
        let read = file.read_to_end(&mut bytes);
        drop(capture_span);
        let result = match read {
            Ok(_) => {
                let classification_span = context.profiler.span(GrepStage::ClassificationWorker);
                let transcoded_bom =
                    bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]);
                let binary = !transcoded_bom && bytes.contains(&0);
                let oversized_line = bytes
                    .split_inclusive(|byte| *byte == b'\n')
                    .any(|line| line.len() > SEARCH_HEAP_BYTES);
                drop(classification_span);
                if binary {
                    sink.mark_binary();
                    Ok(())
                } else if oversized_line {
                    if file.seek(SeekFrom::Start(0)).is_err() {
                        Err(SearchError::Io)
                    } else {
                        context.profiler.record_search_file(context.variant.source);
                        let source_span = context.profiler.span(GrepStage::SearchFileWorker);
                        let std_file = file.into_std();
                        let result = searcher.search_file(context.matcher, &std_file, sink);
                        file = File::from_std(std_file);
                        drop(source_span);
                        result
                    }
                } else {
                    // These bytes were just proven NUL-free, so detection only costs work
                    // here. It must be rearmed afterwards: one searcher serves every
                    // candidate this worker handles, and leaving it disarmed let a later
                    // binary file be searched as text instead of reported as binary.
                    let disarmed = !transcoded_bom;
                    if disarmed {
                        searcher.set_binary_detection(BinaryDetection::none());
                    }
                    context.profiler.record_search_slice();
                    let source_span = context.profiler.span(GrepStage::SearchSliceWorker);
                    let result = searcher.search_slice(context.matcher, &bytes, sink);
                    if disarmed {
                        searcher.set_binary_detection(BinaryDetection::quit(0));
                    }
                    drop(source_span);
                    result
                }
            }
            Err(_) => Err(SearchError::Io),
        };
        drop(memory_permit);
        (result, file)
    } else if use_memory_source {
        context.profiler.record_search_reader();
        let source_span = context.profiler.span(GrepStage::SearchReaderWorker);
        let mut file = file;
        let result = searcher.search_reader(context.matcher, &mut file, sink);
        drop(source_span);
        (result, file)
    } else if use_search_file {
        context.profiler.record_search_file(context.variant.source);
        let source_span = context.profiler.span(GrepStage::SearchFileWorker);
        let file = file.into_std();
        let result = searcher.search_file(context.matcher, &file, sink);
        drop(source_span);
        (result, File::from_std(file))
    } else {
        context.profiler.record_search_reader();
        let source_span = context.profiler.span(GrepStage::SearchReaderWorker);
        let mut file = file;
        let result = searcher.search_reader(context.matcher, &mut file, sink);
        drop(source_span);
        (result, file)
    }
}

/// Every way one candidate's search can end without a result, mapped to what the page
/// reports for it. Cancellation is the only one that fails the whole call.
fn outcome_for_search_error(
    error: SearchError,
    candidate: &Candidate,
    _sink: PlanSink<'_>,
) -> Result<FileOutcome, GrepError> {
    let path = || Some(Arc::clone(&candidate.path));
    match error {
        SearchError::Cancelled => Err(GrepError::Cancelled),
        SearchError::Retired => Ok(FileOutcome::retired()),
        SearchError::CaptureMemory => Ok(FileOutcome::retry(path(), RetryReason::Capture)),
        SearchError::HeapLimit => Ok(FileOutcome::retry(path(), RetryReason::HeapLimit)),
        SearchError::Io => Ok(FileOutcome::skipped(path(), SkipReason::Io)),
        SearchError::Undecodable => Ok(FileOutcome::skipped(path(), SkipReason::Undecodable)),
        SearchError::Transcode(reason) => Ok(FileOutcome::skipped(path(), reason)),
    }
}

const ENCODING_SNIFF_BYTES: usize = 8 * 1024;

/// Decide how one candidate's bytes must be treated before the matcher sees them.
///
/// A prefix that is already valid UTF-8 — which includes every pure-ASCII file — returns
/// `Native` and reaches the unchanged search paths below, so a UTF-8 tree pays only for
/// this one bounded read.
fn classify_source(
    file: &mut File,
    plan: SearchPlan,
    cancellation: &CancellationToken,
) -> Result<SourceHandling, SearchError> {
    if let Some(label) = plan.encoding {
        return Ok(SourceHandling::Transcode(label));
    }
    if cancellation.is_cancelled() {
        return Err(SearchError::Cancelled);
    }
    let mut prefix = vec![0_u8; ENCODING_SNIFF_BYTES];
    let mut filled = 0_usize;
    while filled < prefix.len() {
        match file.read(&mut prefix[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(SearchError::Io),
        }
    }
    let whole_file = filled < prefix.len();
    prefix.truncate(filled);
    if file.seek(SeekFrom::Start(0)).is_err() {
        return Err(SearchError::Io);
    }
    // UTF-16 is transcoded by the searcher's own BOM sniffing, and anything else carrying
    // NUL is binary. Both must keep reaching the existing paths so they are still reported
    // as binary rather than as an encoding problem.
    if prefix.starts_with(&[0xFF, 0xFE]) || prefix.starts_with(&[0xFE, 0xFF]) {
        return Ok(SourceHandling::Native);
    }
    if prefix.contains(&0) {
        return Ok(SourceHandling::Native);
    }
    Ok(
        match crate::encoding::detect_legacy_encoding(&prefix, None, whole_file) {
            Ok(None) if !whole_file && plan.fallback_encoding.is_some() => {
                let valid_utf8 = crate::encoding::decode_stream(
                    &mut *file,
                    None,
                    usize::MAX,
                    cancellation,
                    |_| Ok(crate::encoding::DecodeControl::Continue),
                );
                if file.seek(SeekFrom::Start(0)).is_err() {
                    return Err(SearchError::Io);
                }
                match valid_utf8 {
                    Ok(_) => SourceHandling::Native,
                    Err(crate::encoding::DecodeError::Cancelled) => {
                        return Err(SearchError::Cancelled);
                    }
                    Err(crate::encoding::DecodeError::Io(_)) => return Err(SearchError::Io),
                    Err(_) => SourceHandling::Transcode(
                        plan.fallback_encoding.expect("checked fallback encoding"),
                    ),
                }
            }
            Ok(None) => SourceHandling::Native,
            Ok(Some(label)) => SourceHandling::Transcode(label),
            Err(_) => plan.fallback_encoding.map_or(
                SourceHandling::Skip(SkipReason::Undecodable),
                SourceHandling::Transcode,
            ),
        },
    )
}

/// Search a legacy-encoded candidate by decoding it to UTF-8 first.
///
/// Line structure survives decoding, so sink line numbers still address the source file.
/// Validation and search use fixed buffers and never retain the decoded whole file.
fn search_transcoded(
    file: File,
    label: &'static str,
    context: &FileSearchContext<'_>,
    searcher: &mut Searcher,
    sink: &mut PlanSink<'_>,
) -> (Result<(), SearchError>, File) {
    let Some(encoding) = encoding_rs::Encoding::for_label_no_replacement(label.as_bytes()) else {
        return (Err(SearchError::Transcode(SkipReason::Undecodable)), file);
    };
    context.profiler.record_search_reader();
    context.profiler.record_legacy_stream();
    let source_span = context.profiler.span(GrepStage::SearchReaderWorker);
    searcher.set_binary_detection(BinaryDetection::none());
    let mut reader = crate::encoding::StrictTranscodingReader::new(
        file,
        encoding,
        context.cancellation,
        context.plan.memory.decode_input_bytes,
        context.plan.memory.decode_output_bytes,
    );
    let result = searcher.search_reader(context.matcher, &mut reader, sink);
    let validation = if result.is_ok() {
        io::copy(&mut reader, &mut io::sink()).map(|_| ())
    } else {
        Ok(())
    };
    let (file, failure) = reader.into_parts();
    searcher.set_binary_detection(BinaryDetection::quit(0));
    drop(source_span);
    let result = match failure {
        Some(crate::encoding::TranscodeFailure::Cancelled) => Err(SearchError::Cancelled),
        Some(crate::encoding::TranscodeFailure::Io) => Err(SearchError::Io),
        Some(
            crate::encoding::TranscodeFailure::Malformed
            | crate::encoding::TranscodeFailure::Binary,
        ) => Err(SearchError::Transcode(SkipReason::Undecodable)),
        None if validation.is_err() => Err(SearchError::Io),
        None => result,
    };
    (result, file)
}

fn memory_source_limit(source: GrepSourcePolicy, memory: super::request::GrepMemoryPolicy) -> u64 {
    match source {
        GrepSourcePolicy::Hybrid => memory.memory_source_bytes as u64,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::CaptureLimit(bytes) => bytes,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::Reader
        | GrepSourcePolicy::FileNever
        | GrepSourcePolicy::MmapAlways
        | GrepSourcePolicy::MmapThreshold(_) => 0,
    }
}

#[cfg(any(test, feature = "bench-internals"))]
fn open_identity_candidate(
    access: &FileAccess,
    path: &ResolvedPath,
    profiler: &GrepProfiler,
    fingerprint_stage: GrepStage,
) -> io::Result<OpenedCandidate> {
    if path.is_ambient() && access.symlink_metadata_kind(path)?.is_symlink {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ambient candidate must not be a symbolic link",
        ));
    }
    let open_span = profiler.span(GrepStage::SearchOpenHandleWorker);
    let file = access.open_file_identity(path)?;
    drop(open_span);
    let fingerprint_span = profiler.span(fingerprint_stage);
    let fingerprint = FileFingerprint::from_file(&file)?;
    drop(fingerprint_span);
    if fingerprint.regular {
        Ok(OpenedCandidate { file, fingerprint })
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "candidate is not a regular file",
        ))
    }
}

pub struct OpenedCandidate {
    pub file: File,
    pub fingerprint: FileFingerprint,
}

fn open_candidate(
    access: &FileAccess,
    path: &ResolvedPath,
    profiler: &GrepProfiler,
    fingerprint_stage: GrepStage,
    include_identity: bool,
) -> io::Result<OpenedCandidate> {
    if path.is_ambient() {
        let metadata_span = profiler.span(GrepStage::SearchSymlinkMetadataWorker);
        let metadata = access.symlink_metadata_kind(path);
        drop(metadata_span);
        if metadata?.is_symlink {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ambient candidate must not be a symbolic link",
            ));
        }
    }
    let open_span = profiler.span(GrepStage::SearchOpenHandleWorker);
    let file = access.open_read(path);
    drop(open_span);
    fingerprint_opened_candidate(file?, profiler, fingerprint_stage, include_identity)
}

pub fn fingerprint_opened_candidate(
    file: File,
    profiler: &GrepProfiler,
    fingerprint_stage: GrepStage,
    include_identity: bool,
) -> io::Result<OpenedCandidate> {
    let fingerprint_span = profiler.span(fingerprint_stage);
    let fingerprint = if include_identity {
        FileFingerprint::from_file(&file)?
    } else {
        FileFingerprint::from_file_state(&file)?
    };
    drop(fingerprint_span);
    if fingerprint.regular {
        Ok(OpenedCandidate { file, fingerprint })
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "candidate is not a regular file",
        ))
    }
}

pub fn requires_path_identity(policy: PathnameReopenPolicy) -> bool {
    #[cfg(any(test, feature = "bench-internals"))]
    {
        policy != PathnameReopenPolicy::Off
    }
    #[cfg(not(any(test, feature = "bench-internals")))]
    {
        let _ = policy;
        false
    }
}

impl FileOutcome {
    pub fn skipped(path: Option<Arc<ResolvedPath>>, reason: SkipReason) -> Self {
        Self {
            path,
            records: Vec::new(),
            entries: 0,
            occurrences: 0,
            matched: false,
            skip: Some(reason),
            retired: false,
            leading_skipped: 0,
            retry: None,
        }
    }

    pub fn retired() -> Self {
        Self {
            path: None,
            records: Vec::new(),
            entries: 0,
            occurrences: 0,
            matched: false,
            skip: None,
            retired: true,
            leading_skipped: 0,
            retry: None,
        }
    }

    pub fn retry(path: Option<Arc<ResolvedPath>>, reason: RetryReason) -> Self {
        Self {
            path,
            records: Vec::new(),
            entries: 0,
            occurrences: 0,
            matched: false,
            skip: None,
            retired: false,
            leading_skipped: 0,
            retry: Some(reason),
        }
    }
}

pub struct FilesSink<'a> {
    cancellation: &'a CancellationToken,
    retirement: &'a CancellationToken,
    allow_early_stop: bool,
    matched: bool,
    binary: bool,
}

pub struct CountSink<'a> {
    matcher: &'a RegexMatcher,
    cancellation: &'a CancellationToken,
    retirement: &'a CancellationToken,
    occurrences: usize,
    matched: bool,
    binary: bool,
}

pub struct ContentSink<'a> {
    matcher: &'a RegexMatcher,
    cancellation: &'a CancellationToken,
    retirement: &'a CancellationToken,
    probe: usize,
    allow_early_stop: bool,
    records: Vec<Record>,
    entries: usize,
    occurrences: usize,
    matched: bool,
    binary: bool,
    charged: usize,
    memory_limit: usize,
    same_line_order: BTreeMap<u64, usize>,
    skip_remaining: usize,
    leading_skipped: usize,
}

pub enum PlanSink<'a> {
    Files(FilesSink<'a>),
    Count(CountSink<'a>),
    Content(ContentSink<'a>),
}

#[derive(Clone, Copy)]
pub enum SearchError {
    Cancelled,
    Retired,
    CaptureMemory,
    HeapLimit,
    Undecodable,
    Io,
    /// A legacy-encoded candidate that could not be decoded for searching, carrying the
    /// reason so the skip report says which of the two causes applied.
    Transcode(SkipReason),
}

impl SinkError for SearchError {
    fn error_message<T: std::fmt::Display>(message: T) -> Self {
        let message = message.to_string();
        if message.contains("heap limit") || message.contains("allocation limit") {
            Self::HeapLimit
        } else {
            Self::Io
        }
    }

    fn error_io(error: io::Error) -> Self {
        Self::error_message(error)
    }
}

impl<'a> PlanSink<'a> {
    pub fn new(
        matcher: &'a RegexMatcher,
        plan: SearchPlan,
        cancellation: &'a CancellationToken,
        retirement: &'a CancellationToken,
    ) -> Self {
        match plan.mode {
            GrepMode::Files => Self::Files(FilesSink {
                cancellation,
                retirement,
                allow_early_stop: plan.allow_early_stop,
                matched: false,
                binary: false,
            }),
            GrepMode::Count => Self::Count(CountSink {
                matcher,
                cancellation,
                retirement,
                occurrences: 0,
                matched: false,
                binary: false,
            }),
            GrepMode::Content => Self::Content(ContentSink {
                matcher,
                cancellation,
                retirement,
                probe: plan.probe,
                allow_early_stop: plan.allow_early_stop,
                records: Vec::with_capacity(plan.probe.min(1_024)),
                entries: 0,
                occurrences: 0,
                matched: false,
                binary: false,
                charged: 0,
                memory_limit: plan.memory.capture_bytes,
                same_line_order: BTreeMap::new(),
                skip_remaining: plan.skip,
                leading_skipped: 0,
            }),
        }
    }

    fn finish(self, path: Option<Arc<ResolvedPath>>) -> Result<FileOutcome, GrepError> {
        match self {
            Self::Files(sink) => sink.finish(path),
            Self::Count(sink) => sink.finish(path),
            Self::Content(sink) => sink.finish(path),
        }
    }

    fn mark_binary(&mut self) {
        match self {
            Self::Files(sink) => sink.binary = true,
            Self::Count(sink) => sink.binary = true,
            Self::Content(sink) => sink.binary = true,
        }
    }
}

impl ContentSink<'_> {
    fn capture(
        &mut self,
        line: u64,
        kind: RecordKind,
        order: usize,
        bytes: &[u8],
    ) -> Result<bool, SearchError> {
        self.entries = self.entries.saturating_add(1);
        if self.skip_remaining > 0 {
            self.skip_remaining -= 1;
            self.leading_skipped = self.leading_skipped.saturating_add(1);
            return Ok(true);
        }
        if self.records.len() >= self.probe {
            return Ok(!self.allow_early_stop || self.entries < self.probe);
        }
        let text = std::str::from_utf8(trim_line(bytes)).map_err(|_| SearchError::Undecodable)?;
        let charge = text.len().saturating_add(std::mem::size_of::<Record>());
        if charge > self.memory_limit {
            return self.capture_omission(line, kind, order);
        }
        let charged = self.charged.saturating_add(charge);
        if charged > self.memory_limit {
            return Err(SearchError::CaptureMemory);
        }
        let text = text.to_owned();
        self.charged = charged;
        self.records.push(Record {
            line,
            order,
            kind,
            text,
        });
        Ok(!self.allow_early_stop || self.entries < self.probe)
    }

    fn capture_omission(
        &mut self,
        line: u64,
        kind: RecordKind,
        order: usize,
    ) -> Result<bool, SearchError> {
        let charge = CONTENT_OMISSION
            .len()
            .saturating_add(std::mem::size_of::<Record>());
        let charged = self.charged.saturating_add(charge);
        if charged > self.memory_limit {
            return Err(SearchError::CaptureMemory);
        }
        self.records.push(Record {
            line,
            order,
            kind,
            text: CONTENT_OMISSION.to_owned(),
        });
        self.charged = charged;
        Ok(!self.allow_early_stop || self.entries < self.probe)
    }

    fn finish(mut self, path: Option<Arc<ResolvedPath>>) -> Result<FileOutcome, GrepError> {
        if self.cancellation.is_cancelled() {
            return Err(GrepError::Cancelled);
        }
        if self.binary {
            return Ok(FileOutcome::skipped(path, SkipReason::Binary));
        }
        self.sort_records();
        Ok(FileOutcome {
            path,
            records: self.records,
            entries: self.entries,
            occurrences: self.occurrences,
            matched: self.matched,
            skip: None,
            retired: false,
            leading_skipped: self.leading_skipped,
            retry: None,
        })
    }

    fn sort_records(&mut self) {
        self.records.sort_by(|left, right| {
            left.line
                .cmp(&right.line)
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.order.cmp(&right.order))
        });
    }
}

impl FilesSink<'_> {
    fn finish(self, path: Option<Arc<ResolvedPath>>) -> Result<FileOutcome, GrepError> {
        if self.cancellation.is_cancelled() {
            return Err(GrepError::Cancelled);
        }
        if self.binary {
            return Ok(FileOutcome::skipped(path, SkipReason::Binary));
        }
        Ok(FileOutcome {
            path,
            records: Vec::new(),
            entries: 0,
            occurrences: usize::from(self.matched),
            matched: self.matched,
            skip: None,
            retired: false,
            leading_skipped: 0,
            retry: None,
        })
    }
}

impl CountSink<'_> {
    fn finish(self, path: Option<Arc<ResolvedPath>>) -> Result<FileOutcome, GrepError> {
        if self.cancellation.is_cancelled() {
            return Err(GrepError::Cancelled);
        }
        if self.binary {
            return Ok(FileOutcome::skipped(path, SkipReason::Binary));
        }
        Ok(FileOutcome {
            path,
            records: Vec::new(),
            entries: 0,
            occurrences: self.occurrences,
            matched: self.matched,
            skip: None,
            retired: false,
            leading_skipped: 0,
            retry: None,
        })
    }
}

impl Sink for PlanSink<'_> {
    type Error = SearchError;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        match self {
            Self::Files(sink) => {
                checkpoint(sink.cancellation, sink.retirement)?;
                sink.matched = true;
                Ok(!sink.allow_early_stop)
            }
            Self::Count(sink) => {
                checkpoint(sink.cancellation, sink.retirement)?;
                sink.matched = true;
                let mut count = 0_usize;
                sink.matcher
                    .find_iter(matched.bytes(), |_| {
                        count = count.saturating_add(1);
                        true
                    })
                    .map_err(|_| SearchError::Io)?;
                sink.occurrences = sink.occurrences.saturating_add(count);
                Ok(true)
            }
            Self::Content(sink) => {
                checkpoint(sink.cancellation, sink.retirement)?;
                if sink.allow_early_stop && sink.entries >= sink.probe {
                    return Ok(false);
                }
                sink.matched = true;
                let line = matched.line_number().ok_or(SearchError::Io)?;
                let mut count = 0_usize;
                let remaining = sink.probe.saturating_sub(sink.entries);
                sink.matcher
                    .find_iter(matched.bytes(), |_| {
                        count = count.saturating_add(1);
                        !sink.allow_early_stop || count < remaining
                    })
                    .map_err(|_| SearchError::Io)?;
                sink.occurrences = sink.occurrences.saturating_add(count);
                let start_order = *sink.same_line_order.get(&line).unwrap_or(&0);
                let mut keep_searching = true;
                for order in start_order..start_order.saturating_add(count.max(1)) {
                    keep_searching =
                        sink.capture(line, RecordKind::Match, order, matched.bytes())?;
                    if !keep_searching {
                        break;
                    }
                }
                sink.same_line_order
                    .insert(line, start_order.saturating_add(count.max(1)));
                Ok(keep_searching)
            }
        }
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        let Self::Content(sink) = self else {
            return Ok(true);
        };
        checkpoint(sink.cancellation, sink.retirement)?;
        let line = context.line_number().ok_or(SearchError::Io)?;
        sink.capture(line, RecordKind::Context, 0, context.bytes())
    }

    fn binary_data(&mut self, _searcher: &Searcher, _offset: u64) -> Result<bool, Self::Error> {
        match self {
            Self::Files(sink) => checkpoint(sink.cancellation, sink.retirement)?,
            Self::Count(sink) => checkpoint(sink.cancellation, sink.retirement)?,
            Self::Content(sink) => checkpoint(sink.cancellation, sink.retirement)?,
        }
        self.mark_binary();
        Ok(false)
    }

    fn begin(&mut self, _searcher: &Searcher) -> Result<bool, Self::Error> {
        match self {
            Self::Files(sink) => checkpoint(sink.cancellation, sink.retirement)?,
            Self::Count(sink) => checkpoint(sink.cancellation, sink.retirement)?,
            Self::Content(sink) => checkpoint(sink.cancellation, sink.retirement)?,
        }
        Ok(true)
    }
}

fn checkpoint(
    cancellation: &CancellationToken,
    retirement: &CancellationToken,
) -> Result<(), SearchError> {
    if cancellation.is_cancelled() {
        return Err(SearchError::Cancelled);
    }
    if retirement.is_cancelled() {
        return Err(SearchError::Retired);
    }
    Ok(())
}

fn trim_line(mut bytes: &[u8]) -> &[u8] {
    if bytes.ends_with(b"\n") {
        bytes = &bytes[..bytes.len() - 1];
    }
    if bytes.ends_with(b"\r") {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}
