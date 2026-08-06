#[derive(Clone, Debug)]
struct Candidate {
    path: ResolvedPath,
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
    let mut candidates = Vec::with_capacity(1_024);
    let mut charged = 0_usize;
    let mut terminal_error = None;
    let summary = walk(access, &base, false, cancellation, |entry| {
        if !entry
            .file_type
            .is_some_and(|file_type| file_type.is_file() || file_type.is_symlink())
        {
            return TraversalControl::Continue;
        }
        if glob.is_some_and(|glob| !glob.is_match(entry.key)) {
            return TraversalControl::Continue;
        }
        let path = match access.resolve_traversal_entry(&base, entry.absolute) {
            Ok(path) => path,
            Err(error) => {
                terminal_error = Some(error.into());
                return TraversalControl::Stop;
            }
        };
        match candidate(path) {
            Ok(candidate) => {
                charged = charged
                    .saturating_add(candidate.path.absolute().as_os_str().len())
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
    path.absolute()
        .to_str()
        .ok_or_else(|| GrepError::Validation("candidate path is not valid Unicode".to_owned()))?;
    Ok(Candidate { path })
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
        return Ok(FileOutcome::skipped());
    };
    let mut sink = CollectSink::new(matcher, plan, cancellation);
    match searcher.search_reader(matcher, &mut file, &mut sink) {
        Ok(()) => {}
        Err(SearchError::Cancelled) => return Err(GrepError::Cancelled),
        Err(SearchError::CaptureMemory) => return Err(GrepError::CaptureMemory),
        Err(SearchError::Io) => return Ok(FileOutcome::skipped()),
    }
    after_search();
    let after = FileFingerprint::from_file(&file)?;
    let identity = match open_candidate(access, &candidate.path) {
        Ok(identity) => identity.fingerprint,
        Err(_) => return Ok(FileOutcome::skipped()),
    };
    if before != after || before != identity {
        return Ok(FileOutcome::skipped());
    }
    let absolute = candidate
        .path
        .absolute()
        .to_str()
        .expect("candidate Unicode was validated")
        .to_owned();
    sink.finish(absolute)
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
    fn skipped() -> Self {
        Self {
            absolute: String::new(),
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
            records: Vec::with_capacity(plan.capture_records.min(1_024)),
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
            return Ok(FileOutcome::skipped());
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
