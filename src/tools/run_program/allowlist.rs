use std::{
    env, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::tools::exec::resolve::ResolvedProgram;

pub const ALLOW_PROGRAMS_ENV: &str = "CODEXSHIM_ALLOW_PROGRAMS";

#[derive(Clone, Debug, PartialEq)]
enum Entry {
    /// Pins one exact executable. Stored with the canonical form as well, because resolution
    /// always yields a canonical path while operators write the path they can see.
    Absolute {
        literal: PathBuf,
        canonical: Option<PathBuf>,
    },
    /// Matches a convenient invocation name or the canonical target's file stem.
    Stem(String),
}

/// Programs `run_program` may launch. Empty means deny everything: an operator who wants
/// unrestricted execution is expected to use `bash` instead of a wildcard here.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AllowedPrograms {
    entries: Arc<[Entry]>,
}

impl AllowedPrograms {
    /// Parse a comma-separated allowlist.
    ///
    /// # Errors
    ///
    /// Returns invalid input for an empty item or a relative item containing a path
    /// separator, so a malformed list fails startup rather than a task.
    pub fn parse(value: &str) -> io::Result<Self> {
        if value.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut entries = Vec::new();
        for item in value.split(',') {
            let item = item.trim();
            if item.is_empty() {
                return Err(invalid("allowed program entries must not be empty"));
            }
            let path = Path::new(item);
            if path.is_absolute() {
                entries.push(Entry::Absolute {
                    literal: path.to_owned(),
                    canonical: fs::canonicalize(path).ok(),
                });
                continue;
            }
            if item.contains('/') || item.contains('\\') {
                return Err(invalid(format!(
                    "allowed program {item:?} must be a bare program name or an absolute path"
                )));
            }
            entries.push(Entry::Stem(item.to_owned()));
        }
        Ok(Self {
            entries: entries.into(),
        })
    }

    /// Resolve the allowlist from the environment, used when no startup flag is given.
    ///
    /// # Errors
    ///
    /// Returns invalid input when `CODEXSHIM_ALLOW_PROGRAMS` is not valid Unicode or contains
    /// a malformed entry.
    pub fn from_env() -> io::Result<Self> {
        match env::var_os(ALLOW_PROGRAMS_ENV) {
            None => Ok(Self::default()),
            Some(value) => {
                let value = value
                    .into_string()
                    .map_err(|_| invalid(format!("{ALLOW_PROGRAMS_ENV} must be valid Unicode")))?;
                Self::parse(&value)
            }
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn describe(&self) -> String {
        if self.entries.is_empty() {
            return "(empty; run_program denies every program)".to_owned();
        }
        self.entries
            .iter()
            .map(|entry| match entry {
                Entry::Absolute { literal, .. } => literal.display().to_string(),
                Entry::Stem(name) => name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Absolute entries pin the canonical executable identity. Bare names are an invocation
    /// policy: they may match the resolved alias or its canonical target, which preserves
    /// multicall proxies such as `cargo -> rustup`.
    #[must_use]
    pub(crate) fn permits(&self, resolved: &ResolvedProgram) -> bool {
        let invocation_stem = resolved
            .absolute
            .file_stem()
            .and_then(|value| value.to_str());
        let executable_stem = resolved
            .executable
            .file_stem()
            .and_then(|value| value.to_str());
        self.entries.iter().any(|entry| match entry {
            Entry::Absolute { literal, canonical } => {
                same_path(literal, &resolved.executable)
                    || canonical
                        .as_deref()
                        .is_some_and(|canonical| same_path(canonical, &resolved.executable))
            }
            Entry::Stem(name) => {
                invocation_stem.is_some_and(|stem| same_name(name, stem))
                    || executable_stem.is_some_and(|stem| same_name(name, stem))
            }
        })
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(windows)]
fn same_name(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[cfg(not(windows))]
fn same_name(left: &str, right: &str) -> bool {
    left == right
}

#[cfg(windows)]
fn same_path(left: &Path, right: &Path) -> bool {
    match (left.to_str(), right.to_str()) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => left == right,
    }
}

#[cfg(not(windows))]
fn same_path(left: &Path, right: &Path) -> bool {
    left == right
}
