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
        SEARCH_HEAP_BYTES,
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
pub fn search_file_with(
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
                let (result, returned) = search_memory_bytes(&bytes, file, context, searcher, sink);
                file = returned;
                result
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

/// Search bytes that were leased whole because the file fit the memory source limit.
/// Binary bytes only mark the file, an oversized line falls back to the file source,
/// and proven-clean bytes are searched as a slice with binary detection disarmed.
fn search_memory_bytes(
    bytes: &[u8],
    mut file: File,
    context: &FileSearchContext<'_>,
    searcher: &mut Searcher,
    sink: &mut PlanSink<'_>,
) -> (Result<(), SearchError>, File) {
    let classification_span = context.profiler.span(GrepStage::ClassificationWorker);
    let transcoded_bom = bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]);
    let binary = !transcoded_bom && bytes.contains(&0);
    let oversized_line = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .any(|line| line.len() > SEARCH_HEAP_BYTES);
    drop(classification_span);
    if binary {
        sink.mark_binary();
        return (Ok(()), file);
    }
    if oversized_line {
        if file.seek(SeekFrom::Start(0)).is_err() {
            return (Err(SearchError::Io), file);
        }
        context.profiler.record_search_file(context.variant.source);
        let source_span = context.profiler.span(GrepStage::SearchFileWorker);
        let std_file = file.into_std();
        let result = searcher.search_file(context.matcher, &std_file, sink);
        file = File::from_std(std_file);
        drop(source_span);
        return (result, file);
    }
    // These bytes were just proven NUL-free, so detection only costs work here. It must
    // be rearmed afterwards: one searcher serves every candidate this worker handles,
    // and leaving it disarmed let a later binary file be searched as text instead of
    // being reported as binary.
    let disarmed = !transcoded_bom;
    if disarmed {
        searcher.set_binary_detection(BinaryDetection::none());
    }
    context.profiler.record_search_slice();
    let source_span = context.profiler.span(GrepStage::SearchSliceWorker);
    let result = searcher.search_slice(context.matcher, bytes, sink);
    if disarmed {
        searcher.set_binary_detection(BinaryDetection::quit(0));
    }
    drop(source_span);
    (result, file)
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
mod open;
mod source;

#[cfg(any(test, feature = "bench-internals"))]
use crate::tools::grep::request::PathnameReopenPolicy;
#[cfg(any(test, feature = "bench-internals"))]
use open::open_identity_candidate;
use open::{OpenedCandidate, open_candidate, requires_path_identity};
use source::{SourceHandling, classify_source, memory_source_limit, search_transcoded};

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

/// Fields every sink mode carries regardless of what the plan records.
struct SinkCommon<'a> {
    matcher: &'a RegexMatcher,
    cancellation: &'a CancellationToken,
    retirement: &'a CancellationToken,
    matched: bool,
    binary: bool,
}

/// What one mode accumulates while the searcher reports matches and context.
enum SinkMode {
    Files { allow_early_stop: bool },
    Count { occurrences: usize },
    Content(ContentState),
}

/// The content-mode accumulator: capture, omission, and page-probe accounting.
struct ContentState {
    probe: usize,
    allow_early_stop: bool,
    records: Vec<Record>,
    entries: usize,
    occurrences: usize,
    charged: usize,
    memory_limit: usize,
    same_line_order: BTreeMap<u64, usize>,
    skip_remaining: usize,
    leading_skipped: usize,
}

pub struct PlanSink<'a> {
    common: SinkCommon<'a>,
    mode: SinkMode,
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
        let mode = match plan.mode {
            GrepMode::Files => SinkMode::Files {
                allow_early_stop: plan.allow_early_stop,
            },
            GrepMode::Count => SinkMode::Count { occurrences: 0 },
            GrepMode::Content => SinkMode::Content(ContentState {
                probe: plan.probe,
                allow_early_stop: plan.allow_early_stop,
                records: Vec::with_capacity(plan.probe.min(1_024)),
                entries: 0,
                occurrences: 0,
                charged: 0,
                memory_limit: plan.memory.capture_bytes,
                same_line_order: BTreeMap::new(),
                skip_remaining: plan.skip,
                leading_skipped: 0,
            }),
        };
        Self {
            common: SinkCommon {
                matcher,
                cancellation,
                retirement,
                matched: false,
                binary: false,
            },
            mode,
        }
    }

    /// The content-mode accumulator. Only content-mode callbacks reach it, so any
    /// other mode here is a bug.
    fn content(&mut self) -> &mut ContentState {
        let SinkMode::Content(state) = &mut self.mode else {
            unreachable!("only content mode captures records")
        };
        state
    }

    fn finish(self, path: Option<Arc<ResolvedPath>>) -> Result<FileOutcome, GrepError> {
        if self.common.cancellation.is_cancelled() {
            return Err(GrepError::Cancelled);
        }
        if self.common.binary {
            return Ok(FileOutcome::skipped(path, SkipReason::Binary));
        }
        let (records, entries, occurrences, leading_skipped) = match self.mode {
            SinkMode::Files { .. } => (Vec::new(), 0, usize::from(self.common.matched), 0),
            SinkMode::Count { occurrences, .. } => (Vec::new(), 0, occurrences, 0),
            SinkMode::Content(mut state) => {
                state.records.sort_by(|left, right| {
                    left.line
                        .cmp(&right.line)
                        .then_with(|| left.kind.cmp(&right.kind))
                        .then_with(|| left.order.cmp(&right.order))
                });
                (
                    state.records,
                    state.entries,
                    state.occurrences,
                    state.leading_skipped,
                )
            }
        };
        Ok(FileOutcome {
            path,
            records,
            entries,
            occurrences,
            matched: self.common.matched,
            skip: None,
            retired: false,
            leading_skipped,
            retry: None,
        })
    }

    fn mark_binary(&mut self) {
        self.common.binary = true;
    }
}

impl ContentState {
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

    fn matched(
        &mut self,
        matcher: &RegexMatcher,
        sink_match: &SinkMatch<'_>,
    ) -> Result<bool, SearchError> {
        if self.allow_early_stop && self.entries >= self.probe {
            return Ok(false);
        }
        let line = sink_match.line_number().ok_or(SearchError::Io)?;
        let mut count = 0_usize;
        let remaining = self.probe.saturating_sub(self.entries);
        matcher
            .find_iter(sink_match.bytes(), |_| {
                count = count.saturating_add(1);
                !self.allow_early_stop || count < remaining
            })
            .map_err(|_| SearchError::Io)?;
        self.occurrences = self.occurrences.saturating_add(count);
        let start_order = *self.same_line_order.get(&line).unwrap_or(&0);
        let mut keep_searching = true;
        for order in start_order..start_order.saturating_add(count.max(1)) {
            keep_searching = self.capture(line, RecordKind::Match, order, sink_match.bytes())?;
            if !keep_searching {
                break;
            }
        }
        self.same_line_order
            .insert(line, start_order.saturating_add(count.max(1)));
        Ok(keep_searching)
    }
}

impl Sink for PlanSink<'_> {
    type Error = SearchError;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        checkpoint(self.common.cancellation, self.common.retirement)?;
        self.common.matched = true;
        match &mut self.mode {
            SinkMode::Files { allow_early_stop } => Ok(!*allow_early_stop),
            SinkMode::Count { occurrences } => {
                let mut count = 0_usize;
                self.common
                    .matcher
                    .find_iter(matched.bytes(), |_| {
                        count = count.saturating_add(1);
                        true
                    })
                    .map_err(|_| SearchError::Io)?;
                *occurrences = occurrences.saturating_add(count);
                Ok(true)
            }
            SinkMode::Content { .. } => {
                let regex = self.common.matcher;
                self.content().matched(regex, matched)
            }
        }
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        checkpoint(self.common.cancellation, self.common.retirement)?;
        let line = context.line_number().ok_or(SearchError::Io)?;
        self.content()
            .capture(line, RecordKind::Context, 0, context.bytes())
    }

    fn binary_data(&mut self, _searcher: &Searcher, _offset: u64) -> Result<bool, Self::Error> {
        checkpoint(self.common.cancellation, self.common.retirement)?;
        self.mark_binary();
        Ok(false)
    }

    fn begin(&mut self, _searcher: &Searcher) -> Result<bool, Self::Error> {
        checkpoint(self.common.cancellation, self.common.retirement)?;
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
