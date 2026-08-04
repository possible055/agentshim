use std::{borrow::Cow, io, path::Path};

use ignore::{DirEntry, WalkBuilder};
use tokio_util::sync::CancellationToken;

use crate::path::{RepositoryRoot, ResolvedPath};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraversalControl {
    Continue,
    Stop,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
        (self.skipped() > 0).then(|| {
            format!(
                "Skipped: {} entries (I/O: {}, outside root: {}, non-Unicode: {}).",
                self.skipped(),
                self.io_errors,
                self.escaped_entries,
                self.non_unicode_entries
            )
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TraversalEntry<'a> {
    pub key: &'a Path,
    pub file_type: Option<std::fs::FileType>,
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

/// Traverse one admitted repository directory with the fixed ignore policy.
///
/// Entries are ambient candidates only. Callers that open content must use the
/// root-relative key through [`RepositoryRoot::capability`].
///
/// # Errors
///
/// Returns an error when the root is unavailable, the base is not a directory,
/// or cancellation is requested.
pub fn walk(
    root: &RepositoryRoot,
    base: &ResolvedPath,
    include_ignored: bool,
    cancellation: &CancellationToken,
    mut visitor: impl FnMut(TraversalEntry<'_>) -> TraversalControl,
) -> Result<TraversalSummary, TraversalError> {
    root.verify()?;
    let base_metadata = root.capability().metadata(base.key())?;
    if !base_metadata.is_dir() {
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
    builder.filter_entry(|entry| entry.depth() == 0 || !is_git_entry(entry));

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
        let Some(key) = walked_key(root, entry.path()) else {
            summary.escaped_entries = summary.escaped_entries.saturating_add(1);
            continue;
        };
        if key.to_str().is_none() {
            summary.non_unicode_entries = summary.non_unicode_entries.saturating_add(1);
            continue;
        }
        if visitor(TraversalEntry {
            key: &key,
            file_type: entry.file_type(),
        }) == TraversalControl::Stop
        {
            break;
        }
    }
    Ok(summary)
}

fn walked_key<'a>(root: &RepositoryRoot, path: &'a Path) -> Option<Cow<'a, Path>> {
    if let Ok(key) = path.strip_prefix(root.path())
        && is_relative_key(key)
    {
        return Some(Cow::Borrowed(key));
    }
    root.resolve(path)
        .ok()
        .map(|resolved| Cow::Owned(resolved.key().to_path_buf()))
}

fn is_relative_key(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
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
    use std::{fs, path::Path, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::{TraversalControl, walk};
    use crate::path::RepositoryRoot;

    #[test]
    fn fixed_policy_includes_hidden_respects_ignores_and_excludes_git() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join(".gitignore"), "ignored.txt\n").expect("ignore");
        fs::write(fixture.path().join("visible.txt"), "visible").expect("visible");
        fs::write(fixture.path().join("ignored.txt"), "ignored").expect("ignored");
        fs::write(fixture.path().join(".hidden"), "hidden").expect("hidden");
        fs::create_dir(fixture.path().join(".git")).expect("git");
        fs::write(fixture.path().join(".git/config"), "config").expect("git config");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let base = root.resolve(Path::new(".")).expect("base");
        let mut paths = Vec::new();
        walk(&root, &base, false, &CancellationToken::new(), |entry| {
            let resolved = root.resolve(entry.key).expect("resolve walked key");
            paths.push(resolved.slash_path().expect("model path").to_owned());
            TraversalControl::Continue
        })
        .expect("walk");
        assert!(paths.contains(&"visible.txt".to_owned()));
        assert!(paths.contains(&".hidden".to_owned()));
        assert!(!paths.contains(&"ignored.txt".to_owned()));
        assert!(!paths.iter().any(|path| path.starts_with(".git/")));
    }

    #[test]
    fn cancellation_stops_before_enumeration() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("root");
        let base = root.resolve(Path::new(".")).expect("base");
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(
            walk(&root, &base, false, &cancellation, |_| {
                TraversalControl::Continue
            })
            .is_err()
        );
    }

    #[test]
    fn subdirectory_entries_keep_root_relative_keys() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir(fixture.path().join("src")).expect("src");
        fs::write(fixture.path().join("src/Unicode 界.rs"), "source").expect("source");
        let root = RepositoryRoot::open(fixture.path()).expect("root");
        let base = root.resolve(Path::new("src")).expect("base");
        let mut paths = Vec::new();
        walk(&root, &base, false, &CancellationToken::new(), |entry| {
            paths.push(entry.key.to_path_buf());
            TraversalControl::Continue
        })
        .expect("walk");
        assert_eq!(paths, [Path::new("src/Unicode 界.rs").to_path_buf()]);
    }

    #[cfg(unix)]
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
        let root = RepositoryRoot::open(fixture.path()).expect("root");
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
    /// Requires Windows Developer Mode or symbolic-link privilege to exercise the fixture.
    #[test]
    fn windows_directory_reparse_point_is_not_followed() {
        use std::os::windows::fs::symlink_dir;

        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::write(outside.path().join("secret.txt"), "outside").expect("secret");
        match symlink_dir(outside.path(), fixture.path().join("escape")) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) => {
                eprintln!("directory reparse fixture unavailable: {error}");
                return;
            }
            Err(error) => panic!("directory reparse link: {error}"),
        }
        let root = RepositoryRoot::open(fixture.path()).expect("root");
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
    }
}
