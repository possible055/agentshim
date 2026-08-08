#[derive(Clone, Debug)]
struct Candidate {
    path: Arc<ResolvedPath>,
}

#[allow(clippy::too_many_arguments)]
fn collect_candidates(
    access: &FileAccess,
    input: &str,
    glob: Option<&globset::GlobMatcher>,
    cancellation: &CancellationToken,
    traversal: GrepTraversal,
    #[cfg(any(test, feature = "bench-internals"))] literal_prefix: Option<&Path>,
    policy: CandidatePolicy,
    profiler: &GrepProfiler,
) -> Result<(Vec<Candidate>, TraversalSummary, bool), GrepError> {
    let traversal_span = profiler.span(GrepStage::CandidateTraversal);
    let base = access.resolve(Path::new(input))?;
    if access.metadata_kind(&base)?.is_file {
        let candidate = candidate(base)?;
        let matches = glob.is_none_or(|glob| {
            candidate
                .path
                .slash_path()
                .is_some_and(|path| glob.is_match(path))
        });
        let mut collection = CandidateCollection::new(policy);
        if matches {
            collection.admit(candidate)?;
        }
        profiler.record_candidate_metrics(collection.metrics());
        return Ok((
            collection.candidates,
            TraversalSummary::default(),
            true,
        ));
    }
    let mut activity = None;
    let traversal = match traversal {
        GrepTraversal::Adaptive => {
            let guard = ActiveAdaptiveGrepTraversal::enter();
            let selected = if guard.was_idle && prefer_parallel_candidate_collection(access, &base) {
                GrepTraversal::ParallelBatched
            } else {
                GrepTraversal::Serial
            };
            activity = Some(guard);
            selected
        }
        selected => selected,
    };
    let (mut candidates, summary, metrics) = match traversal {
        GrepTraversal::Adaptive => unreachable!("adaptive traversal was resolved"),
        GrepTraversal::Serial => collect_candidates_serial(
            access,
            &base,
            glob,
            cancellation,
            None,
            policy,
        )?,
        GrepTraversal::ParallelBatched => collect_candidates_parallel(
            access,
            &base,
            glob,
            cancellation,
            None,
            policy,
        )?,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepTraversal::SerialLiteralPrefix => collect_candidates_serial(
            access,
            &base,
            glob,
            cancellation,
            literal_prefix,
            policy,
        )?,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepTraversal::ParallelBatchedLiteralPrefix => collect_candidates_parallel(
            access,
            &base,
            glob,
            cancellation,
            literal_prefix,
            policy,
        )?,
    };
    drop(activity);
    drop(traversal_span);
    let sort_span = profiler.span(GrepStage::CandidateSort);
    sorting::sort_by(&mut candidates, cancellation, |left, right| {
        left.path.sort_key().cmp(right.path.sort_key())
    })
    .map_err(|_| GrepError::Cancelled)?;
    drop(sort_span);
    profiler.record_candidate_metrics(metrics);
    Ok((candidates, summary, false))
}

fn collect_candidates_serial(
    access: &FileAccess,
    base: &ResolvedPath,
    glob: Option<&globset::GlobMatcher>,
    cancellation: &CancellationToken,
    literal_prefix: Option<&Path>,
    policy: CandidatePolicy,
) -> Result<(Vec<Candidate>, TraversalSummary, CandidateMetrics), GrepError> {
    let mut collection = CandidateCollection::new(policy);
    let mut terminal_error = None;
    let mut visit = |entry: crate::traversal::TraversalEntry<'_>| {
        let candidate = match candidate_from_entry(
            access,
            base,
            glob,
            entry.key,
            entry.absolute,
            entry.file_type,
        ) {
            Ok(candidate) => candidate,
            Err(error) => {
                terminal_error = Some(error);
                return TraversalControl::Stop;
            }
        };
        if let Some(candidate) = candidate
            && let Err(error) = collection.admit(candidate)
        {
                terminal_error = Some(error);
                return TraversalControl::Stop;
        }
        TraversalControl::Continue
    };
    let summary = if let Some(literal_prefix) = literal_prefix {
        walk_with_literal_prefix(
            access,
            base,
            false,
            cancellation,
            literal_prefix,
            &mut visit,
        )?
    } else {
        walk(access, base, false, cancellation, &mut visit)?
    };
    if let Some(error) = terminal_error {
        return Err(error);
    }
    let metrics = collection.metrics();
    Ok((collection.candidates, summary, metrics))
}

fn collect_candidates_parallel(
    access: &FileAccess,
    base: &ResolvedPath,
    glob: Option<&globset::GlobMatcher>,
    cancellation: &CancellationToken,
    literal_prefix: Option<&Path>,
    policy: CandidatePolicy,
) -> Result<(Vec<Candidate>, TraversalSummary, CandidateMetrics), GrepError> {
    let collection = Mutex::new(CandidateCollection::new(policy));
    let visit =
        |batch: &[OwnedTraversalEntry]| collect_candidate_batch(access, base, glob, batch, &collection);
    let summary = if let Some(literal_prefix) = literal_prefix {
        walk_parallel_batched_with_literal_prefix(
            access,
            base,
            false,
            cancellation,
            PARALLEL_BATCH_SIZE,
            literal_prefix,
            visit,
        )?
    } else {
        walk_parallel_batched(
            access,
            base,
            false,
            cancellation,
            PARALLEL_BATCH_SIZE,
            visit,
        )?
    };
    let collection = collection
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(error) = collection.terminal_error {
        return Err(error);
    }
    let metrics = collection.metrics();
    Ok((collection.candidates, summary, metrics))
}

fn collect_candidate_batch(
    access: &FileAccess,
    base: &ResolvedPath,
    glob: Option<&globset::GlobMatcher>,
    batch: &[OwnedTraversalEntry],
    collection: &Mutex<CandidateCollection>,
) -> TraversalControl {
    let mut found = Vec::with_capacity(batch.len());
    for entry in batch {
        match candidate_from_entry(
            access,
            base,
            glob,
            &entry.key,
            &entry.absolute,
            entry.file_type,
        ) {
            Ok(Some(candidate)) => found.push(candidate),
            Ok(None) => {}
            Err(error) => {
                let mut collection = collection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                collection.fail(error);
                return TraversalControl::Stop;
            }
        }
    }
    let mut collection = collection
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if collection.terminal_error.is_some() {
        return TraversalControl::Stop;
    }
    for candidate in found {
        if let Err(error) = collection.admit(candidate) {
            collection.fail(error);
            return TraversalControl::Stop;
        }
    }
    TraversalControl::Continue
}

fn candidate_from_entry(
    access: &FileAccess,
    base: &ResolvedPath,
    glob: Option<&globset::GlobMatcher>,
    key: &Path,
    absolute: &Path,
    file_type: Option<std::fs::FileType>,
) -> Result<Option<Candidate>, GrepError> {
    if !file_type.is_some_and(|file_type| file_type.is_file() || file_type.is_symlink())
        || glob.is_some_and(|glob| !glob.is_match(key))
    {
        return Ok(None);
    }
    let path = access.resolve_traversal_entry(base, absolute)?;
    candidate(path).map(Some)
}

struct CandidateCollection {
    candidates: Vec<Candidate>,
    #[cfg(any(test, feature = "bench-internals"))]
    policy: CandidatePolicy,
    estimated_retained_bytes: usize,
    soft_target_crossings: usize,
    key_bytes: usize,
    key_capacity: usize,
    capability_key_bytes: usize,
    capability_key_capacity: usize,
    absolute_bytes: usize,
    absolute_capacity: usize,
    sort_key_bytes: usize,
    sort_key_capacity: usize,
    slash_path_bytes: usize,
    slash_path_capacity: usize,
    terminal_error: Option<GrepError>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default)]
struct CandidateMetrics {
    count: usize,
    estimated_retained_bytes: usize,
    vec_capacity: usize,
    soft_target_crossings: usize,
    key_bytes: usize,
    key_capacity: usize,
    capability_key_bytes: usize,
    capability_key_capacity: usize,
    absolute_bytes: usize,
    absolute_capacity: usize,
    sort_key_bytes: usize,
    sort_key_capacity: usize,
    slash_path_bytes: usize,
    slash_path_capacity: usize,
}

impl CandidateCollection {
    fn new(policy: CandidatePolicy) -> Self {
        #[cfg(not(any(test, feature = "bench-internals")))]
        let _ = policy;
        Self {
            candidates: Vec::with_capacity(1_024),
            #[cfg(any(test, feature = "bench-internals"))]
            policy,
            estimated_retained_bytes: 0,
            soft_target_crossings: 0,
            key_bytes: 0,
            key_capacity: 0,
            capability_key_bytes: 0,
            capability_key_capacity: 0,
            absolute_bytes: 0,
            absolute_capacity: 0,
            sort_key_bytes: 0,
            sort_key_capacity: 0,
            slash_path_bytes: 0,
            slash_path_capacity: 0,
            terminal_error: None,
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn admit(&mut self, candidate: Candidate) -> Result<(), GrepError> {
        let components = candidate.path.memory_components();
        let retained = self
            .estimated_retained_bytes
            .saturating_add(components.key_capacity)
            .saturating_add(components.capability_key_capacity)
            .saturating_add(components.absolute_capacity)
            .saturating_add(components.sort_key_capacity)
            .saturating_add(components.slash_path_capacity)
            .saturating_add(std::mem::size_of::<ResolvedPath>())
            .saturating_add(std::mem::size_of::<Candidate>());
        self.estimated_retained_bytes = retained;
        self.key_bytes = self.key_bytes.saturating_add(components.key_bytes);
        self.key_capacity = self.key_capacity.saturating_add(components.key_capacity);
        self.capability_key_bytes = self
            .capability_key_bytes
            .saturating_add(components.capability_key_bytes);
        self.capability_key_capacity = self
            .capability_key_capacity
            .saturating_add(components.capability_key_capacity);
        self.absolute_bytes = self.absolute_bytes.saturating_add(components.absolute_bytes);
        self.absolute_capacity = self
            .absolute_capacity
            .saturating_add(components.absolute_capacity);
        self.sort_key_bytes = self.sort_key_bytes.saturating_add(components.sort_key_bytes);
        self.sort_key_capacity = self
            .sort_key_capacity
            .saturating_add(components.sort_key_capacity);
        self.slash_path_bytes = self.slash_path_bytes.saturating_add(components.slash_path_bytes);
        self.slash_path_capacity = self
            .slash_path_capacity
            .saturating_add(components.slash_path_capacity);
        self.candidates.push(candidate);
        if retained > CANDIDATE_SOFT_TARGET_BYTES {
            self.soft_target_crossings = self.soft_target_crossings.saturating_add(1);
            #[cfg(any(test, feature = "bench-internals"))]
            if self.policy == CandidatePolicy::FatalCeiling {
                return Err(GrepError::CandidateMemory);
            }
        }
        Ok(())
    }

    fn metrics(&self) -> CandidateMetrics {
        CandidateMetrics {
            count: self.candidates.len(),
            estimated_retained_bytes: self.estimated_retained_bytes,
            vec_capacity: self.candidates.capacity(),
            soft_target_crossings: self.soft_target_crossings,
            key_bytes: self.key_bytes,
            key_capacity: self.key_capacity,
            capability_key_bytes: self.capability_key_bytes,
            capability_key_capacity: self.capability_key_capacity,
            absolute_bytes: self.absolute_bytes,
            absolute_capacity: self.absolute_capacity,
            sort_key_bytes: self.sort_key_bytes,
            sort_key_capacity: self.sort_key_capacity,
            slash_path_bytes: self.slash_path_bytes,
            slash_path_capacity: self.slash_path_capacity,
        }
    }

    fn fail(&mut self, error: GrepError) {
        if self.terminal_error.is_none() {
            self.terminal_error = Some(error);
        }
    }
}

fn prefer_parallel_candidate_collection(access: &FileAccess, base: &ResolvedPath) -> bool {
    if access.root().verify().is_err()
        || (base.is_ambient()
            && access
                .symlink_metadata_kind(base)
                .is_ok_and(|kind| kind.is_symlink))
        || !access.metadata_kind(base).is_ok_and(|kind| kind.is_dir)
    {
        return false;
    }
    std::fs::read_dir(base.absolute()).is_ok_and(|entries| {
        entries.take(PARALLEL_ROOT_ENTRY_THRESHOLD).count()
            >= PARALLEL_ROOT_ENTRY_THRESHOLD
    })
}

struct ActiveAdaptiveGrepTraversal {
    was_idle: bool,
}

impl ActiveAdaptiveGrepTraversal {
    fn enter() -> Self {
        Self {
            was_idle: ACTIVE_ADAPTIVE_GREP_TRAVERSALS.fetch_add(1, AtomicOrdering::AcqRel) == 0,
        }
    }
}

impl Drop for ActiveAdaptiveGrepTraversal {
    fn drop(&mut self) {
        ACTIVE_ADAPTIVE_GREP_TRAVERSALS.fetch_sub(1, AtomicOrdering::AcqRel);
    }
}

fn candidate(path: ResolvedPath) -> Result<Candidate, GrepError> {
    path.absolute()
        .to_str()
        .ok_or_else(|| GrepError::Validation("candidate path is not valid Unicode".to_owned()))?;
    Ok(Candidate {
        path: Arc::new(path),
    })
}

#[derive(Clone, Copy)]
struct SearchPlan {
    mode: GrepMode,
    context: usize,
    capture_records: usize,
}

struct FileSearchContext<'a> {
    access: &'a FileAccess,
    matcher: &'a RegexMatcher,
    plan: SearchPlan,
    cancellation: &'a CancellationToken,
    variant: GrepBenchmarkVariant,
    profiler: &'a GrepProfiler,
    resources: Option<&'a RuntimeResources>,
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
    path: Option<Arc<ResolvedPath>>,
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
    let variant = GrepBenchmarkVariant::default();
    let mut searcher = build_searcher(plan, variant.source);
    let profiler = GrepProfiler::disabled();
    let context = FileSearchContext {
        access,
        matcher,
        plan,
        cancellation,
        variant,
        profiler: &profiler,
        resources: None,
    };
    search_file_with_searcher(candidate, &context, &mut searcher, || {})
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
    let variant = GrepBenchmarkVariant::default();
    let mut searcher = build_searcher(plan, variant.source);
    let profiler = GrepProfiler::disabled();
    let context = FileSearchContext {
        access,
        matcher,
        plan,
        cancellation,
        variant,
        profiler: &profiler,
        resources: None,
    };
    search_file_with_searcher(candidate, &context, &mut searcher, after_search)
}

#[cfg(test)]
fn search_file_with_variant_hook(
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
    let context = FileSearchContext {
        access,
        matcher,
        plan,
        cancellation,
        variant,
        profiler: &profiler,
        resources: None,
    };
    search_file_with_searcher(candidate, &context, &mut searcher, after_search)
}

fn build_searcher(plan: SearchPlan, source: GrepSourcePolicy) -> Searcher {
    let mut builder = SearcherBuilder::new();
    let mmap = match source {
        GrepSourcePolicy::Hybrid => MmapChoice::never(),
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::Reader => MmapChoice::never(),
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
        .heap_limit(Some(SEARCH_HEAP_BYTES))
        .memory_map(mmap)
        .bom_sniffing(true)
        .binary_detection(BinaryDetection::quit(0));
    if plan.mode == GrepMode::Files {
        builder.max_matches(Some(1));
    }
    builder.build()
}

fn search_file_with_searcher(
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
        return Ok(FileOutcome::skipped());
    };
    search_opened_candidate_with_searcher(
        candidate,
        context,
        searcher,
        after_search,
        file,
        &before,
    )
}

fn search_opened_candidate_with_searcher(
    candidate: &Candidate,
    context: &FileSearchContext<'_>,
    searcher: &mut Searcher,
    after_search: impl FnOnce(),
    file: File,
    before: &FileFingerprint,
) -> Result<FileOutcome, GrepError> {
    let mut sink = PlanSink::new(context.matcher, context.plan, context.cancellation);
    let scan_span = context.profiler.span(GrepStage::SearchScanWorker);
    let (search_result, file) = search_source(file, before, context, searcher, &mut sink);
    drop(scan_span);
    match search_result {
        Ok(()) => {}
        Err(SearchError::Cancelled) => return Err(GrepError::Cancelled),
        Err(SearchError::CaptureMemory) => return Err(GrepError::CaptureMemory),
        Err(SearchError::Io) => return Ok(FileOutcome::skipped()),
    }
    after_search();
    let verify_span = context.profiler.span(GrepStage::SearchVerifyWorker);
    let after_span = context.profiler.span(GrepStage::SearchAfterFingerprintWorker);
    let unchanged = before.matches_current_state(&file)?;
    drop(after_span);
    if !unchanged {
        return Ok(FileOutcome::skipped());
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
            Err(_) => return Ok(FileOutcome::skipped()),
        };
        drop(reopen_span);
        if *before != identity {
            return Ok(FileOutcome::skipped());
        }
    }
    drop(verify_span);
    let path = sink
        .matched_file()
        .then(|| Arc::clone(&candidate.path));
    sink.finish(path)
}

fn search_source(
    file: File,
    before: &FileFingerprint,
    context: &FileSearchContext<'_>,
    searcher: &mut Searcher,
    sink: &mut PlanSink<'_>,
) -> (Result<(), SearchError>, File) {
    let use_search_file = match context.variant.source {
        GrepSourcePolicy::Hybrid => true,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::Reader => false,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::FileNever | GrepSourcePolicy::MmapAlways => true,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::MmapThreshold(minimum_bytes) => before.length() >= minimum_bytes,
    };
    let memory_bytes = if context.variant.source == GrepSourcePolicy::Hybrid
        && context.plan.mode == GrepMode::Content
        && before.length() <= MEMORY_SOURCE_BYTES as u64
    {
        usize::try_from(before.length())
            .ok()
            .and_then(|len| context.resources?.try_reserve_memory(len).map(|permit| (len, permit)))
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
                let classification_span =
                    context.profiler.span(GrepStage::ClassificationWorker);
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
                        context
                            .profiler
                            .record_search_file(context.variant.source);
                        let source_span =
                            context.profiler.span(GrepStage::SearchFileWorker);
                        let std_file = file.into_std();
                        let result =
                            searcher.search_file(context.matcher, &std_file, sink);
                        file = File::from_std(std_file);
                        drop(source_span);
                        result
                    }
                } else {
                    if !transcoded_bom {
                        searcher.set_binary_detection(BinaryDetection::none());
                    }
                    context.profiler.record_search_slice();
                    let source_span = context.profiler.span(GrepStage::SearchSliceWorker);
                    let result = searcher.search_slice(context.matcher, &bytes, sink);
                    drop(source_span);
                    result
                }
            }
            Err(_) => Err(SearchError::Io),
        };
        drop(memory_permit);
        (result, file)
    } else if context.variant.source == GrepSourcePolicy::Hybrid
        && context.plan.mode == GrepMode::Content
        && before.length() <= MEMORY_SOURCE_BYTES as u64
    {
        context.profiler.record_search_reader();
        let source_span = context.profiler.span(GrepStage::SearchReaderWorker);
        let mut file = file;
        let result = searcher.search_reader(context.matcher, &mut file, sink);
        drop(source_span);
        (result, file)
    } else if use_search_file {
        context
            .profiler
            .record_search_file(context.variant.source);
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

struct OpenedCandidate {
    file: File,
    fingerprint: FileFingerprint,
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

fn fingerprint_opened_candidate(
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

fn requires_path_identity(policy: PathnameReopenPolicy) -> bool {
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
    fn skipped() -> Self {
        Self {
            path: None,
            records: Vec::new(),
            entries: 0,
            occurrences: 0,
            matched: false,
            skipped: true,
        }
    }
}

struct FilesSink<'a> {
    cancellation: &'a CancellationToken,
    matched: bool,
    binary: bool,
}

struct CountSink<'a> {
    matcher: &'a RegexMatcher,
    cancellation: &'a CancellationToken,
    occurrences: usize,
    matched: bool,
    binary: bool,
}

struct ContentSink<'a> {
    matcher: &'a RegexMatcher,
    cancellation: &'a CancellationToken,
    capture_records: usize,
    records: Vec<Record>,
    entries: usize,
    occurrences: usize,
    matched: bool,
    binary: bool,
    charged: usize,
    same_line_order: BTreeMap<u64, usize>,
}

enum PlanSink<'a> {
    Files(FilesSink<'a>),
    Count(CountSink<'a>),
    Content(ContentSink<'a>),
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

impl<'a> PlanSink<'a> {
    fn new(
        matcher: &'a RegexMatcher,
        plan: SearchPlan,
        cancellation: &'a CancellationToken,
    ) -> Self {
        match plan.mode {
            GrepMode::Files => Self::Files(FilesSink {
                cancellation,
                matched: false,
                binary: false,
            }),
            GrepMode::Count => Self::Count(CountSink {
                matcher,
                cancellation,
                occurrences: 0,
                matched: false,
                binary: false,
            }),
            GrepMode::Content => Self::Content(ContentSink {
                matcher,
                cancellation,
                capture_records: plan.capture_records,
                records: Vec::with_capacity(plan.capture_records.min(1_024)),
                entries: 0,
                occurrences: 0,
                matched: false,
                binary: false,
                charged: 0,
                same_line_order: BTreeMap::new(),
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

    fn matched_file(&self) -> bool {
        match self {
            Self::Files(sink) => sink.matched,
            Self::Count(sink) => sink.matched,
            Self::Content(sink) => sink.matched,
        }
    }

    fn mark_binary(&mut self) {
        match self {
            Self::Files(sink) => sink.binary = true,
            Self::Count(sink) => sink.binary = true,
            Self::Content(sink) => sink.binary = true,
        }
    }

    #[cfg(test)]
    fn capture_capacity(&self) -> usize {
        match self {
            Self::Content(sink) => sink.records.capacity(),
            Self::Files(_) | Self::Count(_) => 0,
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
    ) -> Result<(), SearchError> {
        self.entries = self.entries.saturating_add(1);
        if self.records.len() >= self.capture_records {
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

    fn finish(mut self, path: Option<Arc<ResolvedPath>>) -> Result<FileOutcome, GrepError> {
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
            path,
            records: self.records,
            entries: self.entries,
            occurrences: self.occurrences,
            matched: self.matched,
            skipped: false,
        })
    }
}

impl FilesSink<'_> {
    fn finish(self, path: Option<Arc<ResolvedPath>>) -> Result<FileOutcome, GrepError> {
        if self.cancellation.is_cancelled() {
            return Err(GrepError::Cancelled);
        }
        if self.binary {
            return Ok(FileOutcome::skipped());
        }
        Ok(FileOutcome {
            path,
            records: Vec::new(),
            entries: 0,
            occurrences: usize::from(self.matched),
            matched: self.matched,
            skipped: false,
        })
    }
}

impl CountSink<'_> {
    fn finish(self, path: Option<Arc<ResolvedPath>>) -> Result<FileOutcome, GrepError> {
        if self.cancellation.is_cancelled() {
            return Err(GrepError::Cancelled);
        }
        if self.binary {
            return Ok(FileOutcome::skipped());
        }
        Ok(FileOutcome {
            path,
            records: Vec::new(),
            entries: 0,
            occurrences: self.occurrences,
            matched: self.matched,
            skipped: false,
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
                if sink.cancellation.is_cancelled() {
                    return Err(SearchError::Cancelled);
                }
                sink.matched = true;
                Ok(false)
            }
            Self::Count(sink) => {
                if sink.cancellation.is_cancelled() {
                    return Err(SearchError::Cancelled);
                }
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
                if sink.cancellation.is_cancelled() {
                    return Err(SearchError::Cancelled);
                }
                sink.matched = true;
                let line = matched.line_number().ok_or(SearchError::Io)?;
                let mut count = 0_usize;
                sink.matcher
                    .find_iter(matched.bytes(), |_| {
                        count = count.saturating_add(1);
                        true
                    })
                    .map_err(|_| SearchError::Io)?;
                sink.occurrences = sink.occurrences.saturating_add(count);
                let start_order = *sink.same_line_order.get(&line).unwrap_or(&0);
                for order in start_order..start_order.saturating_add(count.max(1)) {
                    sink.capture(line, RecordKind::Match, order, matched.bytes())?;
                }
                sink.same_line_order
                    .insert(line, start_order.saturating_add(count.max(1)));
                Ok(true)
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
        let line = context.line_number().ok_or(SearchError::Io)?;
        sink.capture(line, RecordKind::Context, 0, context.bytes())?;
        Ok(true)
    }

    fn binary_data(&mut self, _searcher: &Searcher, _offset: u64) -> Result<bool, Self::Error> {
        self.mark_binary();
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

type SearchResult = Result<FileOutcome, GrepError>;

struct ReadySearchBatch {
    start: usize,
    outcomes: VecDeque<SearchResult>,
    _credit: crate::runtime::FileWorkCredit,
    _memory: tokio::sync::OwnedSemaphorePermit,
}

enum PoolSlot {
    Empty,
    Running { start: usize, end: usize },
    Ready(ReadySearchBatch),
}

struct PoolWindow {
    slots: Box<[PoolSlot]>,
    retired: bool,
}

type SharedSearchWindow = Arc<(Mutex<PoolWindow>, Condvar)>;

struct WindowRetirementGuard {
    shared: SharedSearchWindow,
    armed: bool,
}

impl WindowRetirementGuard {
    fn new(shared: SharedSearchWindow) -> Self {
        Self {
            shared,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WindowRetirementGuard {
    fn drop(&mut self) {
        if self.armed {
            retire_window(&self.shared);
        }
    }
}

struct SlotRetirementGuard {
    shared: SharedSearchWindow,
    slot_index: usize,
    start: usize,
    armed: bool,
}

impl SlotRetirementGuard {
    fn new(shared: SharedSearchWindow, slot_index: usize, start: usize) -> Self {
        Self {
            shared,
            slot_index,
            start,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SlotRetirementGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (lock, changed) = &*self.shared;
        let (mut state, poisoned) = match lock.lock() {
            Ok(state) => (state, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        };
        if poisoned {
            state.retired = true;
            for slot in &mut state.slots {
                if matches!(slot, PoolSlot::Running { .. } | PoolSlot::Ready(_)) {
                    *slot = PoolSlot::Empty;
                }
            }
        } else if !state.retired
            && matches!(
                state.slots.get(self.slot_index),
                Some(PoolSlot::Running { start, .. }) if *start == self.start
            )
        {
            state.slots[self.slot_index] = PoolSlot::Empty;
        } else if state.retired {
            for slot in &mut state.slots {
                if matches!(slot, PoolSlot::Running { .. }) {
                    *slot = PoolSlot::Empty;
                }
            }
        }
        changed.notify_all();
    }
}

struct OrderedSearchContext<'a> {
    cancellation: &'a CancellationToken,
    access: &'a Arc<FileAccess>,
    matcher: &'a Arc<RegexMatcher>,
    plan: SearchPlan,
    single_file: bool,
    variant: GrepBenchmarkVariant,
    profiler: &'a GrepProfiler,
    resources: &'a RuntimeResources,
}

#[derive(Clone)]
struct OwnedSearchContext {
    cancellation: CancellationToken,
    access: Arc<FileAccess>,
    matcher: Arc<RegexMatcher>,
    plan: SearchPlan,
    variant: GrepBenchmarkVariant,
    profiler: GrepProfiler,
    resources: RuntimeResources,
}

fn ordered_search(
    candidates: &Arc<[Candidate]>,
    _lanes: usize,
    request: &GrepRequest,
    traversal: TraversalSummary,
    context: &OrderedSearchContext<'_>,
) -> Result<Page, GrepError> {
    let pool = context.resources.file_work_pool();
    let _request = pool.begin_request();
    let shared: SharedSearchWindow = Arc::new((
        Mutex::new(PoolWindow {
            slots: (0..pool.extra_capacity())
                .map(|_| PoolSlot::Empty)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            retired: false,
        }),
        Condvar::new(),
    ));
    let mut retirement = WindowRetirementGuard::new(Arc::clone(&shared));
    let mut page = Page::new(request, traversal);
    let owned = OwnedSearchContext {
        cancellation: context.cancellation.clone(),
        access: Arc::clone(context.access),
        matcher: Arc::clone(context.matcher),
        plan: context.plan,
        variant: context.variant,
        profiler: context.profiler.clone(),
        resources: context.resources.clone(),
    };
    let reduce_span = context.profiler.span(GrepStage::OrderedReduceWall);
    let mut next_dispatch = 0_usize;
    let mut index = 0_usize;
    while index < candidates.len() {
        if context.cancellation.is_cancelled() {
            return Err(GrepError::Cancelled);
        }
        let current_len = search_batch_len(&candidates[index..], context.plan.mode);
        let current_end = index + current_len;
        if next_dispatch <= index {
            next_dispatch = current_end;
        }
        while next_dispatch < candidates.len() {
            let dispatched =
                try_dispatch_batch(next_dispatch, candidates, &owned, &pool, &shared);
            if dispatched == 0 {
                break;
            }
            next_dispatch += dispatched;
        }
        let outcomes = match take_ready_batch(index, context, &shared)? {
            Some(outcomes) => outcomes,
            None => run_inline_batch(&candidates[index..current_end], &owned),
        };
        for outcome in outcomes {
            page.reduce(
                outcome?,
                request.mode.unwrap_or_default(),
                context.single_file,
            )?;
        }
        index = current_end;
    }
    drop(reduce_span);
    retire_window(&shared);
    retirement.disarm();
    Ok(page)
}

fn run_candidate_batch(
    candidates: &[Candidate],
    context: &OwnedSearchContext,
) -> VecDeque<SearchResult> {
    let mut outcomes = VecDeque::with_capacity(candidates.len());
    let mut searcher = build_searcher(context.plan, context.variant.source);
    let file_context = FileSearchContext {
        access: &context.access,
        matcher: &context.matcher,
        plan: context.plan,
        cancellation: &context.cancellation,
        variant: context.variant,
        profiler: &context.profiler,
        resources: Some(&context.resources),
    };
    let Some(first) = candidates.first() else {
        return outcomes;
    };
    let parent_span = context.profiler.span(GrepStage::SearchOpenHandleWorker);
    let reader = context.access.open_same_parent_reader(&first.path);
    drop(parent_span);
    let Ok(reader) = reader else {
        outcomes.extend(
            (0..candidates.len()).map(|_| Ok(FileOutcome::skipped())),
        );
        return outcomes;
    };
    #[cfg(any(test, feature = "bench-internals"))]
    let parent_batch =
        context.variant.pathname_reopen == PathnameReopenPolicy::ParentBatch;
    #[cfg(not(any(test, feature = "bench-internals")))]
    let parent_batch = false;
    let Ok(parent_before) = batch_parent_fingerprint(&reader, parent_batch) else {
        outcomes.extend((0..candidates.len()).map(|_| Ok(FileOutcome::skipped())));
        return outcomes;
    };
    for candidate in candidates {
        if context.cancellation.is_cancelled() {
            outcomes.push_back(Err(GrepError::Cancelled));
            continue;
        }
        outcomes.push_back(run_batch_candidate(
            &reader,
            candidate,
            &file_context,
            &mut searcher,
            parent_batch,
        ));
    }
    if let Some(parent_before) = parent_before {
        validate_batch_parent(&reader, &parent_before, &mut outcomes);
    }
    outcomes
}

#[allow(clippy::unnecessary_wraps)]
fn batch_parent_fingerprint(
    reader: &crate::path::SameParentReader<'_>,
    enabled: bool,
) -> io::Result<Option<FileFingerprint>> {
    #[cfg(not(any(test, feature = "bench-internals")))]
    {
        let _ = (reader, enabled);
        Ok(None)
    }
    #[cfg(any(test, feature = "bench-internals"))]
    {
    if !enabled {
        return Ok(None);
    }
    reader
        .directory()
        .map(FileFingerprint::from_dir)
        .transpose()
    }
}

fn run_batch_candidate(
    reader: &crate::path::SameParentReader<'_>,
    candidate: &Candidate,
    context: &FileSearchContext<'_>,
    searcher: &mut Searcher,
    parent_batch: bool,
) -> SearchResult {
    #[cfg(not(any(test, feature = "bench-internals")))]
    let _ = parent_batch;
    let open_span = context.profiler.span(GrepStage::SearchOpenWorker);
    let handle_span = context.profiler.span(GrepStage::SearchOpenHandleWorker);
    let file = reader.open(&candidate.path);
    drop(handle_span);
    let opened = file.and_then(|file| {
        fingerprint_opened_candidate(
            file,
            context.profiler,
            GrepStage::SearchBeforeFingerprintWorker,
            requires_path_identity(context.variant.pathname_reopen),
        )
    });
    drop(open_span);
    let Ok(OpenedCandidate { file, fingerprint }) = opened else {
        return Ok(FileOutcome::skipped());
    };
    #[cfg(any(test, feature = "bench-internals"))]
    let mut outcome = search_opened_candidate_with_searcher(
        candidate,
        context,
        searcher,
        || {},
        file,
        &fingerprint,
    );
    #[cfg(not(any(test, feature = "bench-internals")))]
    let outcome = search_opened_candidate_with_searcher(
        candidate,
        context,
        searcher,
        || {},
        file,
        &fingerprint,
    );
    #[cfg(any(test, feature = "bench-internals"))]
    if parent_batch && outcome.is_ok() {
        let identity = reader
            .open_identity(&candidate.path)
            .and_then(|file| FileFingerprint::from_file(&file));
        if !identity.is_ok_and(|identity| identity == fingerprint) {
            outcome = Ok(FileOutcome::skipped());
        }
    }
    outcome
}

fn validate_batch_parent(
    reader: &crate::path::SameParentReader<'_>,
    before: &FileFingerprint,
    outcomes: &mut VecDeque<SearchResult>,
) {
    #[cfg(not(any(test, feature = "bench-internals")))]
    {
        let _ = (reader, before, outcomes);
    }
    #[cfg(any(test, feature = "bench-internals"))]
    {
    let after = reader.reopen_parent().and_then(|directory| {
        directory
            .as_ref()
            .map(FileFingerprint::from_dir)
            .transpose()
    });
    if after
        .ok()
        .flatten()
        .is_some_and(|after| before.same_file(&after))
    {
        return;
    }
    for outcome in outcomes {
        if outcome.is_ok() {
            *outcome = Ok(FileOutcome::skipped());
        }
    }
    }
}

fn run_inline_batch(
    candidates: &[Candidate],
    context: &OwnedSearchContext,
) -> VecDeque<SearchResult> {
    let needs_parent_permit = candidates
        .first()
        .is_some_and(|candidate| !candidate.path.is_ambient());
    let parent_open = needs_parent_permit
        .then(|| context.resources.try_acquire_open_file())
        .flatten();
    if !needs_parent_permit || parent_open.is_some() {
        let outcomes = run_candidate_batch(candidates, context);
        drop(parent_open);
        return outcomes;
    }

    let mut searcher = build_searcher(context.plan, context.variant.source);
    #[cfg(any(test, feature = "bench-internals"))]
    let fallback = {
        let mut fallback = context.clone();
        if fallback.variant.pathname_reopen == PathnameReopenPolicy::ParentBatch {
            fallback.variant.pathname_reopen = PathnameReopenPolicy::On;
        }
        fallback
    };
    #[cfg(not(any(test, feature = "bench-internals")))]
    let fallback = context.clone();
    candidates
        .iter()
        .map(|candidate| run_candidate_with_searcher(candidate, &fallback, &mut searcher))
        .collect()
}

fn run_candidate_with_searcher(
    candidate: &Candidate,
    context: &OwnedSearchContext,
    searcher: &mut Searcher,
) -> SearchResult {
    let file_context = FileSearchContext {
        access: &context.access,
        matcher: &context.matcher,
        plan: context.plan,
        cancellation: &context.cancellation,
        variant: context.variant,
        profiler: &context.profiler,
        resources: Some(&context.resources),
    };
    search_file_with_searcher(candidate, &file_context, searcher, || {})
}

fn publish_ready_batch(
    shared: &SharedSearchWindow,
    slot_index: usize,
    index: usize,
    ready: ReadySearchBatch,
    retirement: &mut SlotRetirementGuard,
) {
    let (lock, changed) = &**shared;
    match lock.lock() {
        Ok(mut state) => {
            if !state.retired
                && matches!(
                    state.slots[slot_index],
                    PoolSlot::Running { start, .. } if start == index
                )
            {
                state.slots[slot_index] = PoolSlot::Ready(ready);
                retirement.disarm();
            }
            changed.notify_all();
        }
        Err(poisoned) => {
            let mut state = poisoned.into_inner();
            state.retired = true;
            for slot in &mut state.slots {
                if matches!(slot, PoolSlot::Running { .. } | PoolSlot::Ready(_)) {
                    *slot = PoolSlot::Empty;
                }
            }
            drop(ready);
            changed.notify_all();
        }
    }
}

fn try_dispatch_batch(
    index: usize,
    candidates: &Arc<[Candidate]>,
    context: &OwnedSearchContext,
    pool: &Arc<crate::runtime::FileWorkPool>,
    shared: &SharedSearchWindow,
) -> usize {
    let batch_len = search_batch_len(&candidates[index..], context.plan.mode);
    let Some(credit) = pool.try_credit() else {
        return 0;
    };
    let Some(open_file) = context.resources.try_acquire_open_file() else {
        return 0;
    };
    let parent_open = if candidates[index].path.is_ambient() {
        None
    } else {
        let Some(parent_open) = context.resources.try_acquire_open_file() else {
            return 0;
        };
        Some(parent_open)
    };
    let memory_charge = if context.plan.mode == GrepMode::Content {
        SEARCH_HEAP_BYTES.saturating_mul(batch_len)
    } else {
        SEARCH_HEAP_BYTES
    };
    let Some(memory) = context.resources.try_reserve_memory(memory_charge) else {
        return 0;
    };
    let slot_index = {
        let (lock, _) = &**shared;
        let Ok(mut state) = lock.lock() else {
            retire_window(shared);
            return 0;
        };
        if state.retired {
            return 0;
        }
        let Some(slot_index) = state
            .slots
            .iter()
            .position(|slot| matches!(slot, PoolSlot::Empty))
        else {
            return 0;
        };
        state.slots[slot_index] = PoolSlot::Running {
            start: index,
            end: index + batch_len,
        };
        slot_index
    };
    let batch = Arc::clone(candidates);
    let context = context.clone();
    let job_shared = Arc::clone(shared);
    let job = move |credit| {
        let mut retirement = SlotRetirementGuard::new(
            Arc::clone(&job_shared),
            slot_index,
            index,
        );
        let _worker_activity = GrepWorkerActivity::enter();
        let outcomes = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_candidate_batch(&batch[index..index + batch_len], &context)
        }))
        .unwrap_or_else(|_| {
            (0..batch_len)
                .map(|_| Err(GrepError::Io(io::Error::other("grep worker panicked"))))
                .collect()
        });
        drop(open_file);
        drop(parent_open);
        let ready = ReadySearchBatch {
            start: index,
            outcomes,
            _credit: credit,
            _memory: memory,
        };
        publish_ready_batch(&job_shared, slot_index, index, ready, &mut retirement);
    };
    if pool.spawn(credit, job).is_err() {
        let (lock, changed) = &**shared;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.slots[slot_index] = PoolSlot::Empty;
        changed.notify_all();
        return 0;
    }
    batch_len
}

fn search_batch_len(candidates: &[Candidate], mode: GrepMode) -> usize {
    let Some(first) = candidates.first() else {
        return 0;
    };
    let batch_size = match mode {
        GrepMode::Content => CONTENT_SEARCH_BATCH_SIZE,
        GrepMode::Files | GrepMode::Count => STREAM_SEARCH_BATCH_SIZE,
    };
    candidates
        .iter()
        .take(batch_size)
        .take_while(|candidate| first.path.has_same_parent(&candidate.path))
        .count()
}

fn take_ready_batch(
    index: usize,
    context: &OrderedSearchContext<'_>,
    shared: &SharedSearchWindow,
) -> Result<Option<VecDeque<SearchResult>>, GrepError> {
    let (lock, changed) = &**shared;
    let Ok(mut state) = lock.lock() else {
        retire_window(shared);
        return Err(GrepError::PoolPoison);
    };
    loop {
        if context.cancellation.is_cancelled() {
            return Err(GrepError::Cancelled);
        }
        if state.retired {
            return Err(GrepError::PoolPoison);
        }
        if let Some(slot_index) = state
            .slots
            .iter()
            .position(|slot| matches!(slot, PoolSlot::Ready(ready) if ready.start == index))
        {
            let PoolSlot::Ready(ready) =
                std::mem::replace(&mut state.slots[slot_index], PoolSlot::Empty)
            else {
                unreachable!("checked ready slot remains ready");
            };
            changed.notify_all();
            return Ok(Some(ready.outcomes));
        }
        if state
            .slots
            .iter()
            .any(|slot| {
                matches!(
                    slot,
                    PoolSlot::Running { start, end } if *start <= index && index < *end
                )
            })
        {
            let wait_span = context.profiler.span(GrepStage::OrderedWaitWorker);
            let waited = changed
                .wait_timeout(state, std::time::Duration::from_millis(10))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = waited.0;
            drop(wait_span);
            continue;
        }
        return Ok(None);
    }
}

fn retire_window(shared: &SharedSearchWindow) {
    let (lock, changed) = &**shared;
    let mut state = match lock.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
    state.retired = true;
    for slot in &mut state.slots {
        if matches!(slot, PoolSlot::Running { .. } | PoolSlot::Ready(_)) {
            *slot = PoolSlot::Empty;
        }
    }
    changed.notify_all();
}
