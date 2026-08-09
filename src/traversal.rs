use std::{
    borrow::Cow,
    io,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[cfg(any(test, feature = "bench-internals"))]
use std::path::Component;

use ignore::{DirEntry, WalkBuilder, WalkState};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::path::{FileAccess, ResolvedPath};

const PARALLEL_ROOT_ENTRY_THRESHOLD: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraversalControl {
    Continue,
    Stop,
}

pub(crate) fn prefer_parallel_root(access: &FileAccess, base: &ResolvedPath) -> bool {
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
        entries.take(PARALLEL_ROOT_ENTRY_THRESHOLD).count() >= PARALLEL_ROOT_ENTRY_THRESHOLD
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct TraversalSummary {
    pub io_errors: usize,
    pub escaped_entries: usize,
    pub non_unicode_entries: usize,
}

impl TraversalSummary {
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.io_errors
            .saturating_add(self.escaped_entries)
            .saturating_add(self.non_unicode_entries)
    }

    #[must_use]
    pub fn model_line(&self) -> Option<String> {
        (self.skipped() > 0).then(|| format!("Skipped: {} entries.", self.skipped()))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TraversalEntry<'a> {
    pub key: &'a Path,
    pub absolute: &'a Path,
    pub file_type: Option<std::fs::FileType>,
}

#[derive(Clone, Debug)]
pub struct OwnedTraversalEntry {
    pub key: PathBuf,
    pub absolute: PathBuf,
    pub file_type: Option<std::fs::FileType>,
}

#[derive(Clone, Copy, Debug)]
pub struct ParallelTraversal {
    pub batch_size: usize,
    pub threads: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum TraversalError {
    #[error("traversal cancelled")]
    Cancelled,
    #[error("traversal root must be a directory")]
    NotDirectory,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Traverse one admitted directory with the fixed ignore policy.
///
/// # Errors
///
/// Returns an error when the root is unavailable, the base is not a directory,
/// or cancellation is requested.
pub fn walk(
    access: &FileAccess,
    base: &ResolvedPath,
    include_ignored: bool,
    cancellation: &CancellationToken,
    visitor: impl for<'entry> FnMut(TraversalEntry<'entry>) -> TraversalControl,
) -> Result<TraversalSummary, TraversalError> {
    walk_filtered(access, base, include_ignored, cancellation, None, visitor)
}

pub fn walk_with_literal_prefix(
    access: &FileAccess,
    base: &ResolvedPath,
    include_ignored: bool,
    cancellation: &CancellationToken,
    literal_prefix: &Path,
    visitor: impl for<'entry> FnMut(TraversalEntry<'entry>) -> TraversalControl,
) -> Result<TraversalSummary, TraversalError> {
    walk_filtered(
        access,
        base,
        include_ignored,
        cancellation,
        Some(literal_prefix),
        visitor,
    )
}

fn walk_filtered(
    access: &FileAccess,
    base: &ResolvedPath,
    include_ignored: bool,
    cancellation: &CancellationToken,
    literal_prefix: Option<&Path>,
    mut visitor: impl for<'entry> FnMut(TraversalEntry<'entry>) -> TraversalControl,
) -> Result<TraversalSummary, TraversalError> {
    access.root().verify()?;
    if base.is_ambient() && access.symlink_metadata_kind(base)?.is_symlink {
        return Err(TraversalError::NotDirectory);
    }
    if !access.metadata_kind(base)?.is_dir {
        return Err(TraversalError::NotDirectory);
    }

    let mut builder = WalkBuilder::new(base.absolute());
    builder.follow_links(false).hidden(false).require_git(false);
    if include_ignored {
        builder.standard_filters(false).hidden(false);
    } else {
        builder
            .standard_filters(true)
            .hidden(false)
            .require_git(false);
    }
    configure_entry_filter(&mut builder, access, base, literal_prefix);

    let mut summary = TraversalSummary::default();
    for result in builder.build() {
        if cancellation.is_cancelled() {
            return Err(TraversalError::Cancelled);
        }
        let Ok(entry) = result else {
            summary.io_errors = summary.io_errors.saturating_add(1);
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        let Ok(key) = walked_key(access, base, entry.path()) else {
            summary.escaped_entries = summary.escaped_entries.saturating_add(1);
            continue;
        };
        if key.to_str().is_none() {
            summary.non_unicode_entries = summary.non_unicode_entries.saturating_add(1);
            continue;
        }
        if visitor(TraversalEntry {
            key: &key,
            absolute: entry.path(),
            file_type: entry.file_type(),
        }) == TraversalControl::Stop
        {
            break;
        }
    }
    Ok(summary)
}

/// Traverse one admitted directory with parallel workers and bounded batches.
///
/// # Errors
///
/// Returns an error when the root is unavailable, the base is not a directory,
/// or cancellation is requested.
pub fn walk_parallel_batched(
    access: &FileAccess,
    base: &ResolvedPath,
    include_ignored: bool,
    cancellation: &CancellationToken,
    parallel: ParallelTraversal,
    visitor: impl Fn(&[OwnedTraversalEntry]) -> TraversalControl + Send + Sync,
) -> Result<TraversalSummary, TraversalError> {
    walk_parallel_batched_filtered(
        access,
        base,
        include_ignored,
        cancellation,
        parallel,
        None,
        visitor,
    )
}

pub fn walk_parallel_batched_with_literal_prefix(
    access: &FileAccess,
    base: &ResolvedPath,
    include_ignored: bool,
    cancellation: &CancellationToken,
    parallel: ParallelTraversal,
    literal_prefix: &Path,
    visitor: impl Fn(&[OwnedTraversalEntry]) -> TraversalControl + Send + Sync,
) -> Result<TraversalSummary, TraversalError> {
    walk_parallel_batched_filtered(
        access,
        base,
        include_ignored,
        cancellation,
        parallel,
        Some(literal_prefix),
        visitor,
    )
}

fn walk_parallel_batched_filtered(
    access: &FileAccess,
    base: &ResolvedPath,
    include_ignored: bool,
    cancellation: &CancellationToken,
    parallel: ParallelTraversal,
    literal_prefix: Option<&Path>,
    visitor: impl Fn(&[OwnedTraversalEntry]) -> TraversalControl + Send + Sync,
) -> Result<TraversalSummary, TraversalError> {
    access.root().verify()?;
    if base.is_ambient() && access.symlink_metadata_kind(base)?.is_symlink {
        return Err(TraversalError::NotDirectory);
    }
    if !access.metadata_kind(base)?.is_dir {
        return Err(TraversalError::NotDirectory);
    }
    if cancellation.is_cancelled() {
        return Err(TraversalError::Cancelled);
    }

    let mut builder = WalkBuilder::new(base.absolute());
    builder.follow_links(false).hidden(false).require_git(false);
    if include_ignored {
        builder.standard_filters(false).hidden(false);
    } else {
        builder
            .standard_filters(true)
            .hidden(false)
            .require_git(false);
    }
    configure_entry_filter(&mut builder, access, base, literal_prefix);
    builder.threads(parallel.threads.max(1));

    let summary = AtomicTraversalSummary::default();
    let stopped = AtomicBool::new(false);
    let cancelled = AtomicBool::new(false);
    let remainders = Mutex::new(Vec::new());
    let batch_size = parallel.batch_size.max(1);
    builder.build_parallel().run(|| {
        let summary = &summary;
        let stopped = &stopped;
        let cancelled = &cancelled;
        let visitor = &visitor;
        let mut pending = PendingBatch::new(batch_size, &remainders);
        Box::new(move |result| {
            if stopped.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            if cancellation.is_cancelled() {
                cancelled.store(true, Ordering::Relaxed);
                stopped.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let Ok(entry) = result else {
                summary.io_errors.fetch_add(1, Ordering::Relaxed);
                return WalkState::Continue;
            };
            if entry.depth() == 0 {
                return WalkState::Continue;
            }
            let key = if let Ok(key) = walked_key(access, base, entry.path()) {
                key.into_owned()
            } else {
                summary.escaped_entries.fetch_add(1, Ordering::Relaxed);
                return WalkState::Continue;
            };
            if key.to_str().is_none() {
                summary.non_unicode_entries.fetch_add(1, Ordering::Relaxed);
                return WalkState::Continue;
            }
            pending.entries.push(OwnedTraversalEntry {
                key,
                absolute: entry.path().to_path_buf(),
                file_type: entry.file_type(),
            });
            if pending.entries.len() < pending.capacity {
                return WalkState::Continue;
            }
            if visitor(&pending.take()) == TraversalControl::Stop {
                stopped.store(true, Ordering::Relaxed);
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });

    if cancelled.load(Ordering::Relaxed) || cancellation.is_cancelled() {
        return Err(TraversalError::Cancelled);
    }
    if !stopped.load(Ordering::Relaxed) {
        let remainders = remainders
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for batch in remainders {
            if visitor(&batch) == TraversalControl::Stop {
                break;
            }
        }
    }
    Ok(summary.snapshot())
}

#[must_use]
#[cfg(any(test, feature = "bench-internals"))]
pub fn literal_path_prefix(pattern: &str) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for component in Path::new(pattern).components() {
        let Component::Normal(component) = component else {
            return None;
        };
        if component
            .to_string_lossy()
            .chars()
            .any(|character| matches!(character, '*' | '?' | '[' | ']' | '{' | '}'))
        {
            break;
        }
        prefix.push(component);
    }
    (!prefix.as_os_str().is_empty()).then_some(prefix)
}

fn configure_entry_filter(
    builder: &mut WalkBuilder,
    access: &FileAccess,
    base: &ResolvedPath,
    literal_prefix: Option<&Path>,
) {
    let Some(literal_prefix) = literal_prefix else {
        builder.filter_entry(|entry| entry.depth() == 0 || !is_git_entry(entry));
        return;
    };
    let logical_root = traversal_logical_root(access, base).to_path_buf();
    let literal_prefix = literal_prefix.to_path_buf();
    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        if is_git_entry(entry) {
            return false;
        }
        let Ok(key) = entry.path().strip_prefix(&logical_root) else {
            return true;
        };
        literal_prefix.starts_with(key) || key.starts_with(&literal_prefix)
    });
}

#[derive(Default)]
struct AtomicTraversalSummary {
    io_errors: AtomicUsize,
    escaped_entries: AtomicUsize,
    non_unicode_entries: AtomicUsize,
}

impl AtomicTraversalSummary {
    fn snapshot(&self) -> TraversalSummary {
        TraversalSummary {
            io_errors: self.io_errors.load(Ordering::Relaxed),
            escaped_entries: self.escaped_entries.load(Ordering::Relaxed),
            non_unicode_entries: self.non_unicode_entries.load(Ordering::Relaxed),
        }
    }
}

struct PendingBatch<'a> {
    entries: Vec<OwnedTraversalEntry>,
    capacity: usize,
    remainders: &'a Mutex<Vec<Vec<OwnedTraversalEntry>>>,
}

impl<'a> PendingBatch<'a> {
    fn new(capacity: usize, remainders: &'a Mutex<Vec<Vec<OwnedTraversalEntry>>>) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
            remainders,
        }
    }

    fn take(&mut self) -> Vec<OwnedTraversalEntry> {
        std::mem::replace(&mut self.entries, Vec::with_capacity(self.capacity))
    }
}

impl Drop for PendingBatch<'_> {
    fn drop(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.remainders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(std::mem::take(&mut self.entries));
    }
}

fn walked_key<'path>(
    access: &FileAccess,
    base: &ResolvedPath,
    path: &'path Path,
) -> Result<Cow<'path, Path>, crate::path::PathError> {
    let logical_root = traversal_logical_root(access, base);
    if let Ok(key) = path.strip_prefix(logical_root) {
        return Ok(Cow::Borrowed(if key.as_os_str().is_empty() {
            Path::new(".")
        } else {
            key
        }));
    }
    let resolved = access.resolve_traversal_entry(base, path)?;
    Ok(Cow::Owned(resolved.key().to_path_buf()))
}

fn traversal_logical_root<'a>(access: &'a FileAccess, base: &'a ResolvedPath) -> &'a Path {
    if base.is_external() {
        base.absolute()
    } else {
        access.root().path()
    }
}

fn is_git_entry(entry: &DirEntry) -> bool {
    #[cfg(windows)]
    {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
    }
    #[cfg(not(windows))]
    {
        entry.file_name() == ".git"
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        sync::{Arc, Mutex},
    };

    use tokio_util::sync::CancellationToken;

    use super::{
        ParallelTraversal, TraversalControl, literal_path_prefix, walk, walk_parallel_batched,
        walk_parallel_batched_with_literal_prefix, walk_with_literal_prefix,
    };
    use crate::path::{FileAccess, ReadScope, RepositoryRoot};

    fn access(path: &Path) -> FileAccess {
        access_with_scope(path, ReadScope::Normal)
    }

    fn access_with_scope(path: &Path, scope: ReadScope) -> FileAccess {
        FileAccess::new(Arc::new(RepositoryRoot::open(path).expect("root")), scope)
    }

    #[test]
    fn fixed_policy_includes_hidden_respects_ignores_and_excludes_git() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join(".gitignore"), "ignored.txt\n").expect("ignore");
        fs::write(fixture.path().join("visible.txt"), "visible").expect("visible");
        fs::write(fixture.path().join("ignored.txt"), "ignored").expect("ignored");
        fs::write(fixture.path().join(".hidden"), "hidden").expect("hidden");
        fs::create_dir(fixture.path().join(".git")).expect("git");
        fs::write(fixture.path().join(".git/config"), "config").expect("git config");
        let root = access(fixture.path());
        let base = root.resolve(Path::new(".")).expect("base");
        let mut paths = Vec::new();
        walk(&root, &base, false, &CancellationToken::new(), |entry| {
            paths.push(crate::path::slash_path(entry.key).expect("model path"));
            TraversalControl::Continue
        })
        .expect("walk");
        assert!(paths.contains(&"visible.txt".to_owned()));
        assert!(paths.contains(&".hidden".to_owned()));
        assert!(!paths.contains(&"ignored.txt".to_owned()));
        assert!(!paths.iter().any(|path| path.starts_with(".git/")));
    }

    #[test]
    fn parallel_batches_match_serial_policy_and_summary() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join(".gitignore"), "ignored/**\n").expect("ignore");
        fs::create_dir_all(fixture.path().join("src/deep")).expect("source directories");
        fs::create_dir_all(fixture.path().join("ignored")).expect("ignored directory");
        fs::create_dir_all(fixture.path().join(".git")).expect("git directory");
        for path in [
            "src/a.rs",
            "src/deep/Unicode 界.rs",
            ".hidden.rs",
            "ignored/ignored.rs",
            ".git/internal.rs",
        ] {
            fs::write(fixture.path().join(path), "source").expect("fixture file");
        }
        let root = access(fixture.path());
        let base = root.resolve(Path::new(".")).expect("base");
        let mut serial = Vec::new();
        let serial_summary = walk(&root, &base, false, &CancellationToken::new(), |entry| {
            serial.push(entry.key.to_path_buf());
            TraversalControl::Continue
        })
        .expect("serial walk");
        let parallel = Mutex::new(Vec::new());
        let parallel_summary = walk_parallel_batched(
            &root,
            &base,
            false,
            &CancellationToken::new(),
            ParallelTraversal {
                batch_size: 2,
                threads: 4,
            },
            |batch| {
                parallel
                    .lock()
                    .expect("parallel results")
                    .extend(batch.iter().map(|entry| entry.key.clone()));
                TraversalControl::Continue
            },
        )
        .expect("parallel walk");
        let mut parallel = parallel.into_inner().expect("parallel results");
        serial.sort();
        parallel.sort();
        assert_eq!(parallel, serial);
        assert_eq!(parallel_summary, serial_summary);
    }

    #[test]
    fn literal_prefix_extraction_stops_before_glob_syntax() {
        assert_eq!(
            literal_path_prefix("src/tools/**/*.rs"),
            Some(Path::new("src/tools").to_path_buf())
        );
        assert_eq!(
            literal_path_prefix("src/file.rs"),
            Some(Path::new("src/file.rs").to_path_buf())
        );
        assert_eq!(literal_path_prefix("**/*.rs"), None);
        assert_eq!(literal_path_prefix("{src,tests}/**/*.rs"), None);
        assert_eq!(literal_path_prefix("../src/**/*.rs"), None);
    }

    #[test]
    fn literal_prefix_pruning_preserves_keys_and_ancestor_ignore_rules() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir_all(fixture.path().join("src/deep")).expect("deep");
        fs::create_dir_all(fixture.path().join("src/sibling")).expect("sibling");
        fs::create_dir_all(fixture.path().join("other")).expect("other");
        fs::write(fixture.path().join(".gitignore"), "src/deep/ignored.rs\n").expect("ignore");
        fs::write(fixture.path().join("src/deep/a.rs"), "source").expect("a");
        fs::write(fixture.path().join("src/deep/ignored.rs"), "ignored").expect("ignored");
        fs::write(fixture.path().join("src/sibling/b.rs"), "source").expect("b");
        fs::write(fixture.path().join("other/c.rs"), "source").expect("c");
        let root = access(fixture.path());
        let base = root.resolve(Path::new(".")).expect("base");
        let prefix = Path::new("src/deep");

        let mut serial = Vec::new();
        let serial_summary = walk_with_literal_prefix(
            &root,
            &base,
            false,
            &CancellationToken::new(),
            prefix,
            |entry| {
                serial.push(entry.key.to_path_buf());
                TraversalControl::Continue
            },
        )
        .expect("serial prefix walk");
        let parallel = Mutex::new(Vec::new());
        let parallel_summary = walk_parallel_batched_with_literal_prefix(
            &root,
            &base,
            false,
            &CancellationToken::new(),
            ParallelTraversal {
                batch_size: 2,
                threads: 4,
            },
            prefix,
            |batch| {
                parallel
                    .lock()
                    .expect("parallel prefix results")
                    .extend(batch.iter().map(|entry| entry.key.clone()));
                TraversalControl::Continue
            },
        )
        .expect("parallel prefix walk");
        let mut parallel = parallel.into_inner().expect("parallel prefix results");
        serial.sort();
        parallel.sort();

        assert_eq!(parallel, serial);
        assert_eq!(parallel_summary, serial_summary);
        assert!(serial.contains(&Path::new("src/deep/a.rs").to_path_buf()));
        assert!(!serial.contains(&Path::new("src/deep/ignored.rs").to_path_buf()));
        assert!(!serial.iter().any(|path| path.starts_with("src/sibling")));
        assert!(!serial.iter().any(|path| path.starts_with("other")));
    }

    #[test]
    fn cancellation_stops_before_enumeration() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = access(fixture.path());
        let base = root.resolve(Path::new(".")).expect("base");
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(
            walk(&root, &base, false, &cancellation, |_| {
                TraversalControl::Continue
            })
            .is_err()
        );
        assert!(
            walk_parallel_batched(
                &root,
                &base,
                false,
                &cancellation,
                ParallelTraversal {
                    batch_size: 2,
                    threads: 4,
                },
                |_| TraversalControl::Continue,
            )
            .is_err()
        );
    }

    #[test]
    fn subdirectory_entries_keep_root_relative_keys() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir(fixture.path().join("src")).expect("src");
        fs::write(fixture.path().join("src/Unicode 界.rs"), "source").expect("source");
        let root = access(fixture.path());
        let base = root.resolve(Path::new("src")).expect("base");
        let mut paths = Vec::new();
        walk(&root, &base, false, &CancellationToken::new(), |entry| {
            paths.push(entry.key.to_path_buf());
            TraversalControl::Continue
        })
        .expect("walk");
        assert_eq!(paths, [Path::new("src/Unicode 界.rs").to_path_buf()]);
    }

    #[test]
    fn unrestricted_entries_use_request_relative_keys() {
        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::create_dir(outside.path().join("nested")).expect("nested");
        fs::write(outside.path().join("nested/source.rs"), "source").expect("source");
        let access = access_with_scope(fixture.path(), ReadScope::Unrestricted);
        let base = access.resolve(outside.path()).expect("ambient base");
        let mut paths = Vec::new();
        walk(&access, &base, false, &CancellationToken::new(), |entry| {
            paths.push(entry.key.to_path_buf());
            TraversalControl::Continue
        })
        .expect("ambient walk");
        assert!(paths.contains(&Path::new("nested/source.rs").to_path_buf()));
    }

    #[cfg(unix)]
    #[test]
    fn unrestricted_walk_rejects_an_explicit_symlink_root() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        let target = tempfile::tempdir().expect("target fixture");
        let link = outside.path().join("directory-link");
        symlink(target.path(), &link).expect("directory link");
        let access = access_with_scope(fixture.path(), ReadScope::Unrestricted);
        let base = access.resolve(&link).expect("ambient base");
        assert!(matches!(
            walk(&access, &base, true, &CancellationToken::new(), |_| {
                TraversalControl::Continue
            }),
            Err(super::TraversalError::NotDirectory)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_unicode_entries_remain_in_the_skip_summary() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(
            fixture
                .path()
                .join(OsString::from_vec(vec![b'n', b'o', b'n', 0xFF])),
            "source",
        )
        .expect("non-Unicode source");
        let root = access(fixture.path());
        let base = root.resolve(Path::new(".")).expect("base");
        let mut visited = 0_usize;
        let summary = walk(&root, &base, false, &CancellationToken::new(), |_| {
            visited = visited.saturating_add(1);
            TraversalControl::Continue
        })
        .expect("walk");
        assert_eq!(visited, 0);
        assert_eq!(summary.non_unicode_entries, 1);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an elevated Windows process to create symbolic-link fixtures"]
    fn windows_symlink_directory_reparse_point_is_not_followed() {
        use std::os::windows::fs::symlink_dir;

        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::write(outside.path().join("secret.txt"), "outside").expect("secret");
        symlink_dir(outside.path(), fixture.path().join("escape")).expect("directory reparse link");
        let root = access(fixture.path());
        let base = root.resolve(Path::new(".")).expect("base");
        let mut paths = Vec::new();
        walk(&root, &base, true, &CancellationToken::new(), |entry| {
            paths.push(entry.key.to_path_buf());
            TraversalControl::Continue
        })
        .expect("walk");
        assert!(
            !paths
                .iter()
                .any(|path| path == Path::new("escape/secret.txt"))
        );

        let ambient_link = outside.path().join("ambient-link");
        symlink_dir(fixture.path(), &ambient_link).expect("ambient directory link");
        let access = access_with_scope(fixture.path(), ReadScope::Unrestricted);
        let base = access.resolve(&ambient_link).expect("ambient base");
        assert!(matches!(
            walk(&access, &base, true, &CancellationToken::new(), |_| {
                TraversalControl::Continue
            }),
            Err(super::TraversalError::NotDirectory)
        ));
    }
}
