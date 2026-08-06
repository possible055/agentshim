use std::{cmp::Ordering, collections::BinaryHeap, io, path::Path, sync::Arc};

use globset::GlobBuilder;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    output::{OutputFormatter, OutputLimits},
    path::{FileAccess, PathError, PathSortKey, ResolvedPath},
    sorting,
    traversal::{TraversalControl, TraversalError, TraversalSummary, walk},
};

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1_000;
const MAX_MATCHES: usize = 100_000;
const RETAINED_MEMORY_BYTES: usize = 32 * 1024 * 1024;
const MEMORY_SAFETY_BYTES: usize = 8 * 1024 * 1024;
const PATH_OMISSION: &str = "[glob path omitted: exceeds output budget]";

#[must_use]
pub(crate) fn memory_charge() -> usize {
    RETAINED_MEMORY_BYTES.saturating_add(MEMORY_SAFETY_BYTES)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GlobRequest {
    pub pattern: String,
    pub path: Option<String>,
    pub include_ignored: Option<bool>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

impl GlobRequest {
    /// Validate scalar request constraints before traversal.
    ///
    /// # Errors
    ///
    /// Returns a validation error for empty/NUL values or a limit outside 1..=1,000.
    pub fn validate(&self) -> Result<(), GlobError> {
        if self.pattern.is_empty() {
            return Err(GlobError::Validation(
                "pattern must not be empty".to_owned(),
            ));
        }
        if self.pattern.contains('\0')
            || self.path.as_deref().is_some_and(|path| path.contains('\0'))
        {
            return Err(GlobError::Validation(
                "pattern and path must not contain NUL".to_owned(),
            ));
        }
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(GlobError::Validation(
                "limit must be from 1 to 1000".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GlobError {
    #[error("invalid glob request: {0}")]
    Validation(String),
    #[error("invalid glob pattern: {0}")]
    Pattern(String),
    #[error("more than 100000 paths matched; narrow pattern or path")]
    TooManyMatches,
    #[error("retained glob paths exceed the bounded memory budget; narrow pattern or offset")]
    Memory,
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Traversal(#[from] TraversalError),
    #[error(transparent)]
    Output(#[from] crate::output::OutputError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Find logical-root-relative pattern matches with deterministic Top-K pagination.
///
/// # Errors
///
/// Returns validation, traversal, match-limit, memory, cancellation, or output errors.
pub fn execute(
    access: &Arc<FileAccess>,
    request: &GlobRequest,
    cancellation: &CancellationToken,
) -> Result<String, GlobError> {
    request.validate()?;
    let matcher = GlobBuilder::new(&request.pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map_err(|error| GlobError::Pattern(error.to_string()))?
        .compile_matcher();
    let base_input = request.path.as_deref().unwrap_or(".");
    let base = access.resolve(Path::new(base_input))?;
    let offset = request.offset.unwrap_or(0);
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let retain = offset.saturating_add(limit).min(MAX_MATCHES);
    let mut store = TopK::new(retain);
    let mut total = 0_usize;
    let mut terminal_error = None;
    let summary = walk(
        access,
        &base,
        request.include_ignored.unwrap_or(false),
        cancellation,
        |entry| {
            if !matcher.is_match(entry.path.key()) {
                return TraversalControl::Continue;
            }
            if let Err(error) = record_match(&mut total) {
                terminal_error = Some(error);
                return TraversalControl::Stop;
            }
            if let Err(error) = store.admit(&entry.path) {
                terminal_error = Some(error);
                return TraversalControl::Stop;
            }
            TraversalControl::Continue
        },
    )?;
    if let Some(error) = terminal_error {
        return Err(error);
    }
    let retained = store.into_sorted(cancellation)?;
    render(request, &retained, total, summary, cancellation)
}

fn record_match(total: &mut usize) -> Result<(), GlobError> {
    if *total >= MAX_MATCHES {
        return Err(GlobError::TooManyMatches);
    }
    *total += 1;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GlobMatch {
    sort_key: PathSortKey,
    absolute: String,
    charge: usize,
}

impl Ord for GlobMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key
            .cmp(&other.sort_key)
            .then_with(|| self.absolute.cmp(&other.absolute))
    }
}

impl PartialOrd for GlobMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct TopK {
    capacity: usize,
    heap: BinaryHeap<GlobMatch>,
    charged: usize,
}

impl TopK {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            heap: BinaryHeap::new(),
            charged: 0,
        }
    }

    fn admit(&mut self, path: &ResolvedPath) -> Result<(), GlobError> {
        if self.capacity == 0 {
            return Ok(());
        }
        let absolute = path
            .absolute()
            .to_str()
            .ok_or_else(|| GlobError::Validation("matched path is not valid Unicode".to_owned()))?
            .to_owned();
        let charge = absolute
            .len()
            .saturating_add(path.key().as_os_str().len())
            .saturating_add(std::mem::size_of::<GlobMatch>());
        let candidate = GlobMatch {
            sort_key: path.sort_key().clone(),
            absolute,
            charge,
        };
        if self.heap.len() < self.capacity {
            self.charge(charge)?;
            self.heap.push(candidate);
            return Ok(());
        }
        let Some(worst) = self.heap.peek() else {
            return Ok(());
        };
        if candidate >= *worst {
            return Ok(());
        }
        let new_charge = self
            .charged
            .saturating_sub(worst.charge)
            .saturating_add(charge);
        if new_charge > RETAINED_MEMORY_BYTES {
            return Err(GlobError::Memory);
        }
        self.heap.pop();
        self.heap.push(candidate);
        self.charged = new_charge;
        Ok(())
    }

    fn charge(&mut self, charge: usize) -> Result<(), GlobError> {
        let total = self.charged.saturating_add(charge);
        if total > RETAINED_MEMORY_BYTES {
            return Err(GlobError::Memory);
        }
        self.charged = total;
        Ok(())
    }

    fn into_sorted(self, cancellation: &CancellationToken) -> Result<Vec<GlobMatch>, GlobError> {
        let mut retained = self.heap.into_vec();
        sorting::sort_by(&mut retained, cancellation, Ord::cmp)
            .map_err(|_| TraversalError::Cancelled)?;
        Ok(retained)
    }
}

fn render(
    request: &GlobRequest,
    retained: &[GlobMatch],
    total: usize,
    summary: TraversalSummary,
    cancellation: &CancellationToken,
) -> Result<String, GlobError> {
    let offset = request.offset.unwrap_or(0);
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let available = retained.len().saturating_sub(offset).min(limit);
    let mut cap = available;
    loop {
        let partial = offset.saturating_add(cap) < total;
        let status = if partial {
            continuation(request, offset.saturating_add(cap))?
        } else {
            "Complete.".to_owned()
        };
        let mut tail = Vec::new();
        if let Some(line) = summary.model_line() {
            tail.push(line);
        }
        tail.push(status);
        let mut formatter = OutputFormatter::new(
            format!("Pattern: {}", request.pattern),
            tail,
            OutputLimits::default(),
        )?;
        let mut shown = 0_usize;
        for matched in retained.iter().skip(offset).take(cap) {
            if formatter.try_push_line(&matched.absolute, cancellation)? {
                shown += 1;
                continue;
            }
            if shown == 0 {
                if !formatter.try_push_line(PATH_OMISSION, cancellation)? {
                    return Err(crate::output::OutputError::NoProgress.into());
                }
                shown = 1;
            }
            break;
        }
        if shown == cap {
            return formatter.finish(cancellation).map_err(GlobError::from);
        }
        cap = shown;
    }
}

#[derive(Serialize)]
struct GlobContinuation<'a> {
    pattern: &'a str,
    path: &'a str,
    include_ignored: bool,
    offset: usize,
    limit: usize,
}

fn continuation(request: &GlobRequest, offset: usize) -> Result<String, GlobError> {
    let next = GlobContinuation {
        pattern: &request.pattern,
        path: request.path.as_deref().unwrap_or("."),
        include_ignored: request.include_ignored.unwrap_or(false),
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

    use globset::GlobBuilder;
    use tokio_util::sync::CancellationToken;

    use super::{
        GlobError, GlobMatch, GlobRequest, MAX_MATCHES, PATH_OMISSION, TopK, execute,
        memory_charge, record_match, render,
    };
    use crate::{
        path::{FileAccess, ReadScope, RepositoryRoot, slash_path},
        runtime::MEMORY_BUDGET_BYTES,
        traversal::TraversalSummary,
    };

    fn access(path: &Path) -> Arc<FileAccess> {
        Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(path).expect("root")),
            ReadScope::Normal,
        ))
    }

    fn request(pattern: &str) -> GlobRequest {
        GlobRequest {
            pattern: pattern.to_owned(),
            path: None,
            include_ignored: None,
            offset: None,
            limit: None,
        }
    }

    #[test]
    fn ignore_hidden_git_and_pagination_contract() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join(".gitignore"), "ignored.rs\n").expect("ignore");
        fs::write(fixture.path().join("b.rs"), "b").expect("b");
        fs::write(fixture.path().join("a.rs"), "a").expect("a");
        fs::write(fixture.path().join(".hidden.rs"), "h").expect("hidden");
        fs::write(fixture.path().join("ignored.rs"), "i").expect("ignored");
        fs::create_dir(fixture.path().join(".git")).expect("git");
        fs::write(fixture.path().join(".git/internal.rs"), "g").expect("git file");
        let root = access(fixture.path());
        let mut query = request("*.rs");
        query.limit = Some(2);
        let first = execute(&root, &query, &CancellationToken::new()).expect("glob");
        assert!(first.contains(".hidden.rs"));
        assert!(first.contains("a.rs"));
        assert!(!first.contains("ignored.rs\n"));
        assert!(first.ends_with("\"offset\":2,\"limit\":2}."));

        query.include_ignored = Some(true);
        query.limit = Some(100);
        let all = execute(&root, &query, &CancellationToken::new()).expect("all glob");
        assert!(all.contains("ignored.rs"));
        assert!(!all.contains(".git/internal.rs"));
    }

    #[test]
    fn dense_glob_matches_native_paths_without_prebuilt_slash_strings() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir_all(fixture.path().join("src/nested")).expect("directories");
        for path in ["top.rs", "src/lib.rs", "src/nested/Unicode 界.rs"] {
            fs::write(fixture.path().join(path), "source").expect("source");
        }
        let root = access(fixture.path());
        let mut query = request("**/*");
        query.limit = Some(100);
        let output = execute(&root, &query, &CancellationToken::new()).expect("dense glob");
        for path in ["top.rs", "src/lib.rs", "src/nested/Unicode 界.rs"] {
            let absolute = root.resolve(Path::new(path)).expect("resolved path");
            assert!(output.contains(&absolute.absolute().to_string_lossy().into_owned()));
        }
    }

    #[test]
    fn native_path_matching_equals_slash_path_matching() {
        let native = Path::new("src").join("nested").join("Unicode 界.rs");
        let slash = slash_path(&native).expect("slash path");
        for pattern in ["**/*.rs", "src/**", "**/*", "*.txt"] {
            let matcher = GlobBuilder::new(pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .expect("glob")
                .compile_matcher();
            assert_eq!(
                matcher.is_match(&native),
                matcher.is_match(Path::new(&slash)),
                "pattern {pattern}"
            );
        }
    }

    #[test]
    fn top_k_matches_full_sort_oracle() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("root");
        let mut paths = Vec::new();
        let mut oracle = Vec::new();
        for index in (0..256).rev() {
            let path = format!("file-{index:06}.rs");
            let resolved = root.resolve(Path::new(&path)).expect("resolve");
            oracle.push(resolved.sort_key().clone());
            paths.push(resolved);
        }
        oracle.sort();
        for (offset, limit) in [(0_usize, 17_usize), (57, 31), (246, 10), (257, 5)] {
            let mut top = TopK::new(offset.saturating_add(limit).min(paths.len()));
            for path in &paths {
                top.admit(path).expect("admit");
            }
            let actual = top
                .into_sorted(&CancellationToken::new())
                .expect("sort")
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|entry| entry.sort_key)
                .collect::<Vec<_>>();
            let expected = oracle
                .iter()
                .skip(offset)
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn invalid_pattern_and_match_limit_are_explicit() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = access(fixture.path());
        assert!(matches!(
            execute(&root, &request("["), &CancellationToken::new()),
            Err(GlobError::Pattern(_))
        ));
        let mut total = MAX_MATCHES;
        assert!(matches!(
            record_match(&mut total),
            Err(GlobError::TooManyMatches)
        ));
    }

    #[test]
    fn oversized_path_is_omitted_and_pagination_advances() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("root");
        let first = root.resolve(Path::new("first")).expect("first path");
        let second = root.resolve(Path::new("second")).expect("second path");
        let retained = vec![
            GlobMatch {
                sort_key: first.sort_key().clone(),
                absolute: "x".repeat(crate::output::MODEL_BYTE_LIMIT * 2),
                charge: 0,
            },
            GlobMatch {
                sort_key: second.sort_key().clone(),
                absolute: "second".to_owned(),
                charge: 0,
            },
        ];
        let mut query = request("**/*");
        query.limit = Some(1);

        let first_page = render(
            &query,
            &retained,
            retained.len(),
            TraversalSummary::default(),
            &CancellationToken::new(),
        )
        .expect("first page");
        assert!(first_page.contains(PATH_OMISSION));
        assert!(first_page.contains("\"offset\":1"));

        query.offset = Some(1);
        let second_page = render(
            &query,
            &retained,
            retained.len(),
            TraversalSummary::default(),
            &CancellationToken::new(),
        )
        .expect("second page");
        assert!(second_page.contains("second"));
        assert!(second_page.ends_with("Complete."));
    }

    #[test]
    fn runtime_memory_charge_includes_safety_margin() {
        assert_eq!(memory_charge(), 40 * 1024 * 1024);
        assert!(memory_charge() <= MEMORY_BUDGET_BYTES);
    }
}
