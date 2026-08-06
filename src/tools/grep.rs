use std::{
    collections::BTreeMap,
    io::{self},
    path::Path,
    sync::{Arc, Condvar, Mutex},
};

use cap_std::fs::File;
use globset::GlobBuilder;
use grep_matcher::{LineTerminator, Matcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, MmapChoice, Searcher, SearcherBuilder, Sink, SinkContext, SinkError, SinkMatch,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    output::{MODEL_BYTE_LIMIT, OutputFormatter, OutputLimits},
    path::{FileAccess, PathError, ResolvedPath},
    sorting,
    tools::read::FileFingerprint,
    traversal::{TraversalControl, TraversalError, TraversalSummary, walk},
};

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1_000;
const MAX_CONTEXT: usize = 20;
const CANDIDATE_MEMORY_BYTES: usize = 8 * 1024 * 1024;
const SEARCH_HEAP_BYTES: usize = 1024 * 1024;
const CAPTURE_MEMORY_BYTES: usize = 1024 * 1024;
const PAGE_MEMORY_BYTES: usize = MODEL_BYTE_LIMIT;
const MEMORY_SAFETY_BYTES: usize = 8 * 1024 * 1024;
const ORDERED_WINDOW_FACTOR: usize = 1;
const GENERIC_OMISSION: &str = "[grep result omitted: exceeds output budget]";
const CONTENT_OMISSION: &str = "[line text omitted: exceeds output budget]";

#[must_use]
pub(crate) fn memory_charge(lanes: usize) -> usize {
    CANDIDATE_MEMORY_BYTES
        .saturating_add(
            lanes.saturating_mul(SEARCH_HEAP_BYTES.saturating_add(CAPTURE_MEMORY_BYTES)),
        )
        .saturating_add(PAGE_MEMORY_BYTES)
        .saturating_add(MEMORY_SAFETY_BYTES)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum GrepMode {
    #[default]
    Content,
    Files,
    Count,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CaseMode {
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrepRequest {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub mode: Option<GrepMode>,
    pub fixed_strings: Option<bool>,
    pub case: Option<CaseMode>,
    pub context_lines: Option<usize>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

impl GrepRequest {
    /// Validate scalar constraints before regex compilation or filesystem I/O.
    ///
    /// # Errors
    ///
    /// Returns a validation error for NUL, context above 20, or limit outside 1..=1,000.
    pub fn validate(&self) -> Result<(), GrepError> {
        if self.pattern.contains('\0')
            || self.path.as_deref().is_some_and(|path| path.contains('\0'))
            || self.glob.as_deref().is_some_and(|glob| glob.contains('\0'))
        {
            return Err(GrepError::Validation(
                "pattern, path, and glob must not contain NUL".to_owned(),
            ));
        }
        if self.context_lines.unwrap_or(0) > MAX_CONTEXT {
            return Err(GrepError::Validation(
                "context_lines must be from 0 to 20".to_owned(),
            ));
        }
        if !(1..=MAX_LIMIT).contains(&self.limit.unwrap_or(DEFAULT_LIMIT)) {
            return Err(GrepError::Validation(
                "limit must be from 1 to 1000".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GrepError {
    #[error("invalid grep request: {0}")]
    Validation(String),
    #[error("Rust regex compile error: {0}; lookaround and backreferences are not supported")]
    Regex(String),
    #[error("invalid grep glob: {0}")]
    Glob(String),
    #[error("grep candidates exceed the bounded memory budget; narrow path or glob")]
    CandidateMemory,
    #[error("grep matching content exceeds the bounded capture budget; narrow the query")]
    CaptureMemory,
    #[error("grep cancelled")]
    Cancelled,
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Traversal(#[from] TraversalError),
    #[error(transparent)]
    Output(#[from] crate::output::OutputError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Search policy-admitted files with an ordered bounded worker window.
///
/// # Errors
///
/// Returns validation, regex, traversal, resource, cancellation, I/O, or output errors.
pub fn execute(
    access: &Arc<FileAccess>,
    request: &GrepRequest,
    lanes: usize,
    cancellation: &CancellationToken,
) -> Result<String, GrepError> {
    request.validate()?;
    let matcher = Arc::new(build_matcher(request)?);
    let glob = request
        .glob
        .as_deref()
        .map(|pattern| {
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .map(|glob| glob.compile_matcher())
                .map_err(|error| GrepError::Glob(error.to_string()))
        })
        .transpose()?;
    let (candidates, traversal_summary, single_file) = collect_candidates(
        access,
        request.path.as_deref().unwrap_or("."),
        glob.as_ref(),
        cancellation,
    )
    .map_err(normalize_cancellation)?;
    let needed = request
        .offset
        .unwrap_or(0)
        .saturating_add(request.limit.unwrap_or(DEFAULT_LIMIT))
        .saturating_add(1);
    let plan = SearchPlan {
        mode: request.mode.unwrap_or_default(),
        context: request.context_lines.unwrap_or(0),
        capture_records: needed,
    };
    let lanes = lanes.clamp(1, candidates.len().max(1));
    let access = Arc::clone(access);
    let context = OrderedSearchContext {
        cancellation,
        access: &access,
        matcher: &matcher,
        plan,
        single_file,
    };
    let page = ordered_search(&candidates, lanes, request, traversal_summary, &context)?;
    render(request, &page, cancellation)
}

fn normalize_cancellation(error: GrepError) -> GrepError {
    if matches!(error, GrepError::Traversal(TraversalError::Cancelled)) {
        GrepError::Cancelled
    } else {
        error
    }
}

fn build_matcher(request: &GrepRequest) -> Result<RegexMatcher, GrepError> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .fixed_strings(request.fixed_strings.unwrap_or(false))
        .crlf(true)
        .ban_byte(Some(0));
    match request.case.unwrap_or_default() {
        CaseMode::Smart => {
            builder.case_smart(true);
        }
        CaseMode::Sensitive => {
            builder.case_insensitive(false);
        }
        CaseMode::Insensitive => {
            builder.case_insensitive(true);
        }
    }
    builder
        .build(&request.pattern)
        .map_err(|error| GrepError::Regex(error.to_string()))
}

#[derive(Clone, Debug)]
struct Candidate {
    path: ResolvedPath,
    absolute: String,
}

fn collect_candidates(
    access: &FileAccess,
    input: &str,
    glob: Option<&globset::GlobMatcher>,
    cancellation: &CancellationToken,
) -> Result<(Vec<Candidate>, TraversalSummary, bool), GrepError> {
    let base = access.resolve(Path::new(input))?;
    if access.metadata_kind(&base)?.is_file {
        let candidate = candidate(base)?;
        let matches = glob.is_none_or(|glob| {
            candidate
                .path
                .slash_path()
                .is_some_and(|path| glob.is_match(path))
        });
        return Ok((
            matches.then_some(candidate).into_iter().collect(),
            TraversalSummary::default(),
            true,
        ));
    }
    let mut candidates = Vec::new();
    let mut charged = 0_usize;
    let mut terminal_error = None;
    let summary = walk(access, &base, false, cancellation, |entry| {
        if !entry
            .file_type
            .is_some_and(|file_type| file_type.is_file() || file_type.is_symlink())
        {
            return TraversalControl::Continue;
        }
        if glob.is_some_and(|glob| !glob.is_match(entry.path.key())) {
            return TraversalControl::Continue;
        }
        match candidate(entry.path) {
            Ok(candidate) => {
                charged = charged
                    .saturating_add(candidate.absolute.len())
                    .saturating_add(std::mem::size_of::<Candidate>());
                if charged > CANDIDATE_MEMORY_BYTES {
                    terminal_error = Some(GrepError::CandidateMemory);
                    return TraversalControl::Stop;
                }
                candidates.push(candidate);
            }
            Err(error) => {
                terminal_error = Some(error);
                return TraversalControl::Stop;
            }
        }
        TraversalControl::Continue
    })?;
    if let Some(error) = terminal_error {
        return Err(error);
    }
    sorting::sort_by(&mut candidates, cancellation, |left, right| {
        left.path.sort_key().cmp(right.path.sort_key())
    })
    .map_err(|_| GrepError::Cancelled)?;
    Ok((candidates, summary, false))
}

fn candidate(path: ResolvedPath) -> Result<Candidate, GrepError> {
    let absolute = path
        .absolute()
        .to_str()
        .ok_or_else(|| GrepError::Validation("candidate path is not valid Unicode".to_owned()))?
        .to_owned();
    Ok(Candidate { path, absolute })
}

#[derive(Clone, Copy)]
struct SearchPlan {
    mode: GrepMode,
    context: usize,
    capture_records: usize,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum RecordKind {
    Context,
    Match,
}

#[derive(Debug)]
struct Record {
    line: u64,
    order: usize,
    kind: RecordKind,
    text: String,
}

#[derive(Debug)]
struct FileOutcome {
    absolute: String,
    records: Vec<Record>,
    entries: usize,
    occurrences: usize,
    matched: bool,
    skipped: bool,
}

#[cfg(test)]
fn search_file(
    access: &FileAccess,
    candidate: &Candidate,
    matcher: &RegexMatcher,
    plan: SearchPlan,
    cancellation: &CancellationToken,
) -> Result<FileOutcome, GrepError> {
    let mut searcher = build_searcher(plan);
    search_file_with_searcher(
        access,
        candidate,
        matcher,
        plan,
        cancellation,
        &mut searcher,
        || {},
    )
}

#[cfg(test)]
fn search_file_with_hook(
    access: &FileAccess,
    candidate: &Candidate,
    matcher: &RegexMatcher,
    plan: SearchPlan,
    cancellation: &CancellationToken,
    after_search: impl FnOnce(),
) -> Result<FileOutcome, GrepError> {
    let mut searcher = build_searcher(plan);
    search_file_with_searcher(
        access,
        candidate,
        matcher,
        plan,
        cancellation,
        &mut searcher,
        after_search,
    )
}

fn build_searcher(plan: SearchPlan) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(true)
        .line_terminator(LineTerminator::crlf())
        .before_context(plan.context)
        .after_context(plan.context)
        .heap_limit(Some(SEARCH_HEAP_BYTES))
        .memory_map(MmapChoice::never())
        .bom_sniffing(true)
        .binary_detection(BinaryDetection::quit(0));
    if plan.mode == GrepMode::Files {
        builder.max_matches(Some(1));
    }
    builder.build()
}

fn search_file_with_searcher(
    access: &FileAccess,
    candidate: &Candidate,
    matcher: &RegexMatcher,
    plan: SearchPlan,
    cancellation: &CancellationToken,
    searcher: &mut Searcher,
    after_search: impl FnOnce(),
) -> Result<FileOutcome, GrepError> {
    if cancellation.is_cancelled() {
        return Err(GrepError::Cancelled);
    }
    let Ok(OpenedCandidate {
        mut file,
        fingerprint: before,
    }) = open_candidate(access, &candidate.path)
    else {
        return Ok(FileOutcome::skipped(&candidate.absolute));
    };
    let mut sink = CollectSink::new(matcher, plan, cancellation);
    match searcher.search_reader(matcher, &mut file, &mut sink) {
        Ok(()) => {}
        Err(SearchError::Cancelled) => return Err(GrepError::Cancelled),
        Err(SearchError::CaptureMemory) => return Err(GrepError::CaptureMemory),
        Err(SearchError::Io) => return Ok(FileOutcome::skipped(&candidate.absolute)),
    }
    after_search();
    let after = FileFingerprint::from_file(&file)?;
    let identity = match open_candidate(access, &candidate.path) {
        Ok(identity) => identity.fingerprint,
        Err(_) => return Ok(FileOutcome::skipped(&candidate.absolute)),
    };
    if before != after || before != identity {
        return Ok(FileOutcome::skipped(&candidate.absolute));
    }
    sink.finish(candidate.absolute.clone())
}

struct OpenedCandidate {
    file: File,
    fingerprint: FileFingerprint,
}

fn open_candidate(access: &FileAccess, path: &ResolvedPath) -> io::Result<OpenedCandidate> {
    if path.is_ambient() && access.symlink_metadata_kind(path)?.is_symlink {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ambient candidate must not be a symbolic link",
        ));
    }
    let file = access.open_read(path)?;
    let fingerprint = FileFingerprint::from_file(&file)?;
    if fingerprint.regular {
        Ok(OpenedCandidate { file, fingerprint })
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "candidate is not a regular file",
        ))
    }
}

impl FileOutcome {
    fn skipped(absolute: &str) -> Self {
        Self {
            absolute: absolute.to_owned(),
            records: Vec::new(),
            entries: 0,
            occurrences: 0,
            matched: false,
            skipped: true,
        }
    }
}

struct CollectSink<'a> {
    matcher: &'a RegexMatcher,
    plan: SearchPlan,
    cancellation: &'a CancellationToken,
    records: Vec<Record>,
    entries: usize,
    occurrences: usize,
    matched: bool,
    binary: bool,
    charged: usize,
    same_line_order: BTreeMap<u64, usize>,
}

enum SearchError {
    Cancelled,
    CaptureMemory,
    Io,
}

impl SinkError for SearchError {
    fn error_message<T: std::fmt::Display>(_message: T) -> Self {
        Self::Io
    }

    fn error_io(_error: io::Error) -> Self {
        Self::Io
    }
}

impl<'a> CollectSink<'a> {
    fn new(
        matcher: &'a RegexMatcher,
        plan: SearchPlan,
        cancellation: &'a CancellationToken,
    ) -> Self {
        Self {
            matcher,
            plan,
            cancellation,
            records: Vec::new(),
            entries: 0,
            occurrences: 0,
            matched: false,
            binary: false,
            charged: 0,
            same_line_order: BTreeMap::new(),
        }
    }

    fn capture(
        &mut self,
        line: u64,
        kind: RecordKind,
        order: usize,
        bytes: &[u8],
    ) -> Result<(), SearchError> {
        self.entries = self.entries.saturating_add(1);
        if self.records.len() >= self.plan.capture_records {
            return Ok(());
        }
        let text = std::str::from_utf8(trim_line(bytes))
            .map_err(|_| SearchError::Io)?
            .to_owned();
        self.charged = self
            .charged
            .saturating_add(text.len())
            .saturating_add(std::mem::size_of::<Record>());
        if self.charged > CAPTURE_MEMORY_BYTES {
            return Err(SearchError::CaptureMemory);
        }
        self.records.push(Record {
            line,
            order,
            kind,
            text,
        });
        Ok(())
    }

    fn finish(mut self, absolute: String) -> Result<FileOutcome, GrepError> {
        if self.cancellation.is_cancelled() {
            return Err(GrepError::Cancelled);
        }
        if self.binary {
            return Ok(FileOutcome::skipped(&absolute));
        }
        self.records.sort_by(|left, right| {
            left.line
                .cmp(&right.line)
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.order.cmp(&right.order))
        });
        Ok(FileOutcome {
            absolute,
            records: self.records,
            entries: self.entries,
            occurrences: self.occurrences,
            matched: self.matched,
            skipped: false,
        })
    }
}

impl Sink for CollectSink<'_> {
    type Error = SearchError;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.cancellation.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        self.matched = true;
        let line = matched.line_number().ok_or(SearchError::Io)?;
        let mut count = 0_usize;
        self.matcher
            .find_iter(matched.bytes(), |_| {
                count = count.saturating_add(1);
                true
            })
            .map_err(|_| SearchError::Io)?;
        self.occurrences = self.occurrences.saturating_add(count);
        if self.plan.mode == GrepMode::Content {
            let start_order = *self.same_line_order.get(&line).unwrap_or(&0);
            for order in start_order..start_order.saturating_add(count.max(1)) {
                self.capture(line, RecordKind::Match, order, matched.bytes())?;
            }
            self.same_line_order
                .insert(line, start_order.saturating_add(count.max(1)));
        }
        Ok(self.plan.mode != GrepMode::Files)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        if self.plan.mode != GrepMode::Content {
            return Ok(true);
        }
        let line = context.line_number().ok_or(SearchError::Io)?;
        self.capture(line, RecordKind::Context, 0, context.bytes())?;
        Ok(true)
    }

    fn binary_data(&mut self, _searcher: &Searcher, _offset: u64) -> Result<bool, Self::Error> {
        self.binary = true;
        Ok(false)
    }
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

struct WindowState<T> {
    next_dispatch: usize,
    next_reduce: usize,
    results: BTreeMap<usize, T>,
    stop: bool,
}

type SearchResult = Result<FileOutcome, GrepError>;
type SharedSearchWindow = Arc<(Mutex<WindowState<SearchResult>>, Condvar)>;

struct OrderedSearchContext<'a> {
    cancellation: &'a CancellationToken,
    access: &'a Arc<FileAccess>,
    matcher: &'a Arc<RegexMatcher>,
    plan: SearchPlan,
    single_file: bool,
}

fn ordered_search(
    candidates: &[Candidate],
    lanes: usize,
    request: &GrepRequest,
    traversal: TraversalSummary,
    context: &OrderedSearchContext<'_>,
) -> Result<Page, GrepError> {
    let shared: SharedSearchWindow = Arc::new((
        Mutex::new(WindowState {
            next_dispatch: 0,
            next_reduce: 0,
            results: BTreeMap::new(),
            stop: false,
        }),
        Condvar::new(),
    ));
    let window = lanes.saturating_mul(ORDERED_WINDOW_FACTOR).max(1);
    let mut page = Page::new(request, traversal);
    std::thread::scope(|scope| -> Result<(), GrepError> {
        for _ in 0..lanes {
            let shared = Arc::clone(&shared);
            let access = Arc::clone(context.access);
            let matcher = Arc::clone(context.matcher);
            let cancellation = context.cancellation.clone();
            let plan = context.plan;
            scope.spawn(move || {
                let mut searcher = build_searcher(plan);
                loop {
                    let index = {
                        let (lock, changed) = &*shared;
                        let mut state = lock
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        while !state.stop
                            && state.next_dispatch < candidates.len()
                            && state.next_dispatch >= state.next_reduce.saturating_add(window)
                        {
                            state = changed
                                .wait(state)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        if state.stop || state.next_dispatch >= candidates.len() {
                            return;
                        }
                        let index = state.next_dispatch;
                        state.next_dispatch += 1;
                        index
                    };
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        search_file_with_searcher(
                            &access,
                            &candidates[index],
                            &matcher,
                            plan,
                            &cancellation,
                            &mut searcher,
                            || {},
                        )
                    }))
                    .unwrap_or_else(|_| {
                        Err(GrepError::Io(io::Error::other("grep worker panicked")))
                    });
                    let (lock, changed) = &*shared;
                    let mut state = lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.results.insert(index, outcome);
                    changed.notify_all();
                }
            });
        }

        reduce_ordered_results(candidates.len(), request, context, &shared, &mut page)?;
        stop_window(&shared);
        Ok(())
    })?;
    Ok(page)
}

fn reduce_ordered_results(
    candidate_count: usize,
    request: &GrepRequest,
    context: &OrderedSearchContext<'_>,
    shared: &SharedSearchWindow,
    page: &mut Page,
) -> Result<(), GrepError> {
    for index in 0..candidate_count {
        if context.cancellation.is_cancelled() {
            stop_window(shared);
            return Err(GrepError::Cancelled);
        }
        let outcome = {
            let (lock, changed) = &**shared;
            let mut state = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !state.results.contains_key(&index) {
                state = changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            let outcome = state.results.remove(&index).expect("ordered result exists");
            state.next_reduce = index.saturating_add(1);
            changed.notify_all();
            outcome
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                stop_window(shared);
                return Err(error);
            }
        };
        if let Err(error) = page.reduce(
            outcome,
            request.mode.unwrap_or_default(),
            context.single_file,
        ) {
            stop_window(shared);
            return Err(error);
        }
    }
    Ok(())
}

fn stop_window<T>(shared: &Arc<(Mutex<WindowState<T>>, Condvar)>) {
    let (lock, changed) = &**shared;
    let mut state = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.stop = true;
    changed.notify_all();
}

struct PageLine {
    text: String,
    fallback: Option<String>,
}

impl PageLine {
    fn charge(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.text.len())
            .saturating_add(self.fallback.as_deref().map_or(0, str::len))
    }
}

struct Page {
    lines: Vec<PageLine>,
    total: usize,
    skipped: usize,
    offset: usize,
    retain: usize,
    charged: usize,
    retaining: bool,
    traversal: TraversalSummary,
}

impl Page {
    fn new(request: &GrepRequest, traversal: TraversalSummary) -> Self {
        Self {
            lines: Vec::new(),
            total: 0,
            skipped: 0,
            offset: request.offset.unwrap_or(0),
            retain: request.limit.unwrap_or(DEFAULT_LIMIT).saturating_add(1),
            charged: 0,
            retaining: true,
            traversal,
        }
    }

    fn reduce(
        &mut self,
        outcome: FileOutcome,
        mode: GrepMode,
        single_file: bool,
    ) -> Result<(), GrepError> {
        if outcome.skipped {
            self.skipped = self.skipped.saturating_add(1);
            if single_file {
                return Err(GrepError::Io(io::Error::other(
                    "single grep target changed, is binary, or could not be searched",
                )));
            }
            return Ok(());
        }
        match mode {
            GrepMode::Files if outcome.matched => {
                self.push_entry(outcome.absolute, None);
            }
            GrepMode::Count if outcome.matched => {
                self.push_entry(
                    format!("{}:{}", outcome.absolute, outcome.occurrences),
                    None,
                );
            }
            GrepMode::Content => {
                let entries = outcome.entries;
                let captured = outcome.records.len();
                for record in outcome.records {
                    let separator = if record.kind == RecordKind::Match {
                        ':'
                    } else {
                        '-'
                    };
                    let fallback = format!(
                        "{}{separator}{}{separator}{CONTENT_OMISSION}",
                        outcome.absolute, record.line
                    );
                    self.push_entry(
                        format!(
                            "{}{separator}{}{separator}{}",
                            outcome.absolute, record.line, record.text
                        ),
                        Some(fallback),
                    );
                }
                self.total = self.total.saturating_add(entries.saturating_sub(captured));
            }
            GrepMode::Files | GrepMode::Count => {}
        }
        Ok(())
    }

    fn push_entry(&mut self, line: String, detailed_fallback: Option<String>) {
        if self.total >= self.offset && self.lines.len() < self.retain && self.retaining {
            let fallback = detailed_fallback.unwrap_or_else(|| GENERIC_OMISSION.to_owned());
            if self.can_retain(&line, Some(&fallback)) {
                self.retain_line(PageLine {
                    text: line,
                    fallback: Some(fallback),
                });
            } else if fallback != GENERIC_OMISSION
                && self.can_retain(&fallback, Some(GENERIC_OMISSION))
            {
                self.retain_line(PageLine {
                    text: fallback,
                    fallback: Some(GENERIC_OMISSION.to_owned()),
                });
            } else if self.can_retain(GENERIC_OMISSION, None) {
                self.retain_line(PageLine {
                    text: GENERIC_OMISSION.to_owned(),
                    fallback: None,
                });
            } else {
                self.retaining = false;
            }
        }
        self.total = self.total.saturating_add(1);
    }

    fn can_retain(&self, text: &str, fallback: Option<&str>) -> bool {
        let charge = std::mem::size_of::<PageLine>()
            .saturating_add(text.len())
            .saturating_add(fallback.map_or(0, str::len));
        self.charged.saturating_add(charge) <= PAGE_MEMORY_BYTES
    }

    fn retain_line(&mut self, line: PageLine) {
        self.charged = self.charged.saturating_add(line.charge());
        self.lines.push(line);
    }
}

fn render(
    request: &GrepRequest,
    page: &Page,
    cancellation: &CancellationToken,
) -> Result<String, GrepError> {
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let available = page.lines.len().min(limit);
    let mut cap = available;
    loop {
        let partial = page.offset.saturating_add(cap) < page.total;
        let status = if partial {
            continuation(request, page.offset.saturating_add(cap))?
        } else {
            "Complete.".to_owned()
        };
        let mut tail = Vec::new();
        let total_skipped = page.skipped.saturating_add(page.traversal.skipped());
        if total_skipped > 0 {
            tail.push(format!("Skipped: {total_skipped} files or entries."));
        }
        tail.push(status);
        let mut formatter = OutputFormatter::new(
            format!("Pattern: {}", request.pattern),
            tail,
            OutputLimits::default(),
        )?;
        let mut shown = 0_usize;
        for line in page.lines.iter().take(cap) {
            if formatter.try_push_line(&line.text, cancellation)? {
                shown += 1;
                continue;
            }
            if shown == 0 {
                let Some(fallback) = line.fallback.as_deref() else {
                    return Err(crate::output::OutputError::NoProgress.into());
                };
                if !formatter.try_push_line(fallback, cancellation)? {
                    return Err(crate::output::OutputError::NoProgress.into());
                }
                shown = 1;
            }
            break;
        }
        if shown == cap {
            return formatter.finish(cancellation).map_err(GrepError::from);
        }
        cap = shown;
    }
}

#[derive(Serialize)]
struct GrepContinuation<'a> {
    pattern: &'a str,
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    glob: Option<&'a str>,
    mode: GrepMode,
    fixed_strings: bool,
    case: CaseMode,
    context_lines: usize,
    offset: usize,
    limit: usize,
}

fn continuation(request: &GrepRequest, offset: usize) -> Result<String, GrepError> {
    let next = GrepContinuation {
        pattern: &request.pattern,
        path: request.path.as_deref().unwrap_or("."),
        glob: request.glob.as_deref(),
        mode: request.mode.unwrap_or_default(),
        fixed_strings: request.fixed_strings.unwrap_or(false),
        case: request.case.unwrap_or_default(),
        context_lines: request.context_lines.unwrap_or(0),
        offset,
        limit: request.limit.unwrap_or(DEFAULT_LIMIT),
    };
    let encoded = serde_json::to_string(&next)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(format!("Partial: continue with {encoded}."))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::{
        CaseMode, GENERIC_OMISSION, GrepError, GrepMode, GrepRequest, PAGE_MEMORY_BYTES, Page,
        SearchPlan, build_matcher, candidate, execute, memory_charge, render, search_file,
        search_file_with_hook,
    };
    use crate::{
        path::{FileAccess, ReadScope, RepositoryRoot},
        runtime::MEMORY_BUDGET_BYTES,
    };

    fn request(pattern: &str) -> GrepRequest {
        GrepRequest {
            pattern: pattern.to_owned(),
            path: None,
            glob: Some("**/*.rs".to_owned()),
            mode: None,
            fixed_strings: None,
            case: None,
            context_lines: None,
            offset: None,
            limit: None,
        }
    }

    fn fixture() -> (tempfile::TempDir, Arc<FileAccess>) {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir(fixture.path().join("src")).expect("src");
        fs::write(
            fixture.path().join("src/a.rs"),
            "before\nNeedle needle\nafter\n",
        )
        .expect("a");
        fs::write(fixture.path().join("src/b.rs"), "needle\nnone\nneedle\n").expect("b");
        fs::write(fixture.path().join("ignored.rs"), "needle\n").expect("ignored");
        fs::write(fixture.path().join(".gitignore"), "ignored.rs\n").expect("gitignore");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        (fixture, root)
    }

    fn native_path(path: &str) -> String {
        path.replace('/', std::path::MAIN_SEPARATOR_STR)
    }

    #[test]
    fn content_fixed_case_context_and_worker_counts_are_deterministic() {
        let (_fixture, root) = fixture();
        let mut query = request("needle");
        query.fixed_strings = Some(true);
        query.case = Some(CaseMode::Insensitive);
        query.context_lines = Some(1);
        let cancellation = CancellationToken::new();
        let baseline = execute(&root, &query, 1, &cancellation).expect("grep");
        assert!(baseline.contains(&native_path("src/a.rs")));
        assert!(baseline.contains("-1-before"));
        assert!(!baseline.contains("ignored.rs"));
        for workers in [2, 4, 8, 16] {
            assert_eq!(
                execute(&root, &query, workers, &cancellation).expect("parallel grep"),
                baseline
            );
        }
    }

    #[test]
    fn grep_without_glob_avoids_path_conversion_and_remains_deterministic() {
        let (_fixture, root) = fixture();
        let mut query = request("needle");
        query.glob = None;
        query.fixed_strings = Some(true);
        let cancellation = CancellationToken::new();
        let baseline = execute(&root, &query, 1, &cancellation).expect("grep without glob");
        assert!(baseline.contains(&native_path("src/a.rs")));
        assert!(baseline.contains(&native_path("src/b.rs")));
        assert!(!baseline.contains("ignored.rs"));
        for workers in [2, 4, 8, 16] {
            assert_eq!(
                execute(&root, &query, workers, &cancellation).expect("parallel grep"),
                baseline
            );
        }
    }

    #[test]
    fn files_count_and_pagination_modes() {
        let (_fixture, root) = fixture();
        let cancellation = CancellationToken::new();
        let mut files = request("needle");
        files.mode = Some(GrepMode::Files);
        files.limit = Some(1);
        let output = execute(&root, &files, 4, &cancellation).expect("files");
        assert!(output.contains(&native_path("src/a.rs")));
        assert!(output.contains("\"offset\":1"));

        let mut count = request("needle");
        count.mode = Some(GrepMode::Count);
        let output = execute(&root, &count, 4, &cancellation).expect("count");
        assert!(output.contains(&format!("{}:2", native_path("src/a.rs"))));
        assert!(output.contains(&format!("{}:2", native_path("src/b.rs"))));
    }

    #[test]
    fn invalid_regex_lookaround_backreference_binary_and_utf16() {
        let (fixture, root) = fixture();
        let cancellation = CancellationToken::new();
        for pattern in ["(", "(?=needle)", r"(needle)\1"] {
            assert!(matches!(
                execute(&root, &request(pattern), 1, &cancellation),
                Err(GrepError::Regex(_))
            ));
        }
        fs::write(fixture.path().join("src/binary.rs"), b"needle\0needle").expect("binary");
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "needle\n".encode_utf16() {
            utf16.extend(unit.to_le_bytes());
        }
        fs::write(fixture.path().join("src/utf16.rs"), utf16).expect("utf16");
        let output = execute(&root, &request("needle"), 2, &cancellation).expect("grep");
        assert!(output.contains("utf16.rs"));
        assert!(output.contains("Skipped: 1"));
    }

    #[test]
    fn long_line_is_bounded_and_large_file_remains_searchable() {
        let (fixture, root) = fixture();
        let mut long = String::from("needle");
        long.push_str(&"x".repeat(2 * 1024 * 1024));
        fs::write(fixture.path().join("src/long.rs"), long).expect("long line");
        let mut large = "ordinary line\n".repeat(128);
        large.push_str("needle at the end\n");
        fs::write(fixture.path().join("src/large.rs"), large).expect("large file");

        let output =
            execute(&root, &request("needle"), 4, &CancellationToken::new()).expect("bounded grep");
        assert!(output.contains(&native_path("src/large.rs")));
        assert!(output.contains("Skipped: 1"));
    }

    #[test]
    fn oversized_first_result_is_omitted_and_pagination_advances() {
        let (fixture, root) = fixture();
        fs::create_dir(fixture.path().join("paging")).expect("paging directory");
        let mut large = String::from("needle ");
        large.push_str(&"x".repeat(crate::output::MODEL_BYTE_LIMIT * 2));
        large.push('\n');
        fs::write(fixture.path().join("paging/0-large.rs"), large).expect("large match");
        fs::write(fixture.path().join("paging/z-small.rs"), "needle small\n").expect("small match");
        let mut query = request("needle");
        query.path = Some("paging".to_owned());
        query.fixed_strings = Some(true);
        query.limit = Some(1);

        let first = execute(&root, &query, 1, &CancellationToken::new()).expect("first page");
        assert!(first.contains("[line text omitted: exceeds output budget]"));
        assert!(first.contains("\"offset\":1"));

        query.offset = Some(1);
        let second = execute(&root, &query, 1, &CancellationToken::new()).expect("second page");
        assert!(second.contains("z-small.rs"));
        assert!(second.contains("needle small"));
    }

    #[test]
    fn page_retention_stays_within_its_memory_budget() {
        let query = request("needle");
        let mut page = Page::new(&query, crate::traversal::TraversalSummary::default());
        for index in 0..1_000 {
            page.push_entry(
                format!("{index}:{}", "x".repeat(20_000)),
                Some(format!(
                    "{index}:[line text omitted: exceeds output budget]"
                )),
            );
        }

        assert!(page.charged <= PAGE_MEMORY_BYTES);
        assert_eq!(page.total, 1_000);
        assert!(!page.retaining);
        assert!(page.lines.iter().any(|line| line.text == GENERIC_OMISSION));
        let output = render(&query, &page, &CancellationToken::new()).expect("bounded page");
        assert!(output.contains("Partial:"));
    }

    #[test]
    fn runtime_memory_charge_is_conservative_and_bounded() {
        assert!(memory_charge(1) > 16 * 1024 * 1024);
        assert!(memory_charge(16) > memory_charge(1));
        assert!(memory_charge(16) <= MEMORY_BUDGET_BYTES);
    }

    #[test]
    fn capture_memory_limit_is_reported() {
        let (fixture, root) = fixture();
        let line = format!("needle {}\n", "x".repeat(2_000));
        fs::write(fixture.path().join("src/a-large.rs"), line.repeat(600))
            .expect("large matching fixture");
        let mut query = request("needle");
        query.fixed_strings = Some(true);
        query.limit = Some(1_000);

        assert!(matches!(
            execute(&root, &query, 1, &CancellationToken::new()),
            Err(GrepError::CaptureMemory)
        ));
    }

    #[test]
    fn traversal_cancellation_uses_the_grep_cancellation_error() {
        let (_fixture, root) = fixture();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            execute(&root, &request("needle"), 1, &cancellation),
            Err(GrepError::Cancelled)
        ));
    }

    #[test]
    fn file_change_race_skips_only_that_file_with_deterministic_summary() {
        let (fixture, root) = fixture();
        let cancellation = CancellationToken::new();
        let query = request("needle");
        let matcher = build_matcher(&query).expect("matcher");
        let plan = SearchPlan {
            mode: GrepMode::Content,
            context: 0,
            capture_records: 10,
        };
        let changed = candidate(root.resolve(Path::new("src/a.rs")).expect("changed path"))
            .expect("changed candidate");
        let stable = candidate(root.resolve(Path::new("src/b.rs")).expect("stable path"))
            .expect("stable candidate");
        let changed_outcome =
            search_file_with_hook(&root, &changed, &matcher, plan, &cancellation, || {
                fs::write(
                    fixture.path().join("src/a.rs"),
                    "replacement without the old match\n",
                )
                .expect("replace during search");
            })
            .expect("changed outcome");
        assert!(changed_outcome.skipped);
        let stable_outcome =
            search_file(&root, &stable, &matcher, plan, &cancellation).expect("stable outcome");

        let mut page = Page::new(&query, crate::traversal::TraversalSummary::default());
        page.reduce(changed_outcome, GrepMode::Content, false)
            .expect("reduce changed");
        page.reduce(stable_outcome, GrepMode::Content, false)
            .expect("reduce stable");
        let output = render(&query, &page, &cancellation).expect("render");
        assert!(output.contains(&native_path("src/b.rs")));
        assert!(!output.contains("replacement without"));
        assert!(output.contains("Skipped: 1 files or entries."));
    }
}
