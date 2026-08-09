use std::{
    collections::HashMap,
    env, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use super::ProcessError;

#[derive(Clone, Debug)]
struct SearchEntry {
    directory: PathBuf,
    canonical: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ProcessResolver {
    search_path: Arc<[SearchEntry]>,
    resolved: Arc<RwLock<HashMap<String, ResolvedProgram>>>,
    cacheable: bool,
    #[cfg(windows)]
    path_extensions: Arc<[String]>,
}

impl ProcessResolver {
    #[must_use]
    pub fn capture() -> Self {
        let entries = env::var_os("PATH")
            .map(|path| {
                env::split_paths(&path)
                    .filter(|entry| !entry.as_os_str().is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Self::from_search_path(entries)
    }

    fn from_search_path(entries: Vec<PathBuf>) -> Self {
        // Relative PATH entries resolve against the request cwd, so their canonical form is
        // not knowable at capture time and per-program caching would be cwd-dependent.
        let cacheable = entries.iter().all(|entry| entry.is_absolute());
        let search_path = entries
            .into_iter()
            .map(|directory| {
                let canonical = directory
                    .is_absolute()
                    .then(|| fs::canonicalize(&directory).ok())
                    .flatten();
                SearchEntry {
                    directory,
                    canonical,
                }
            })
            .collect::<Vec<_>>()
            .into();
        Self {
            search_path,
            resolved: Arc::new(RwLock::new(HashMap::new())),
            cacheable,
            #[cfg(windows)]
            path_extensions: controlled_path_extensions(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests(search_path: Vec<PathBuf>) -> Self {
        Self::from_search_path(search_path)
    }

    pub(crate) fn resolve(
        &self,
        program: &str,
        cwd: &Path,
    ) -> Result<ResolvedProgram, ProcessError> {
        let requested = Path::new(program);
        if requested.is_absolute() {
            return resolve_candidate(requested, None).map_err(|failure| failure.error);
        }
        if has_separator(program) {
            return resolve_candidate(&cwd.join(requested), None).map_err(|failure| failure.error);
        }
        let key = self.cache_key(program);
        if let Some(key) = &key
            && let Some(hit) = self
                .resolved
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(key)
        {
            return Ok(hit.clone());
        }
        let resolved = self.search(program, cwd)?;
        if let Some(key) = key {
            self.resolved
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, resolved.clone());
        }
        Ok(resolved)
    }

    fn search(&self, program: &str, cwd: &Path) -> Result<ResolvedProgram, ProcessError> {
        for entry in self.search_path.iter() {
            let directory = if entry.directory.is_absolute() {
                entry.directory.clone()
            } else {
                cwd.join(&entry.directory)
            };
            #[cfg(not(windows))]
            let candidates = Self::candidates(&directory, program);
            #[cfg(windows)]
            let candidates = self.candidates(&directory, program);
            for candidate in candidates {
                match resolve_candidate(&candidate, entry.canonical.as_deref()) {
                    Ok(resolved) => return Ok(resolved),
                    Err(failure) if failure.probed => return Err(failure.error),
                    Err(_) => {}
                }
            }
        }
        Err(ProcessError::Resolve(format!(
            "program {program:?} was not found in the captured PATH"
        )))
    }

    /// Only successful PATH searches are cached, so a tool installed mid-session is still
    /// found on a later call.
    fn cache_key(&self, program: &str) -> Option<String> {
        if !self.cacheable {
            return None;
        }
        #[cfg(windows)]
        {
            Some(program.to_ascii_lowercase())
        }
        #[cfg(not(windows))]
        {
            Some(program.to_owned())
        }
    }

    #[cfg(not(windows))]
    fn candidates(directory: &Path, program: &str) -> Vec<PathBuf> {
        vec![directory.join(program)]
    }

    #[cfg(windows)]
    fn candidates(&self, directory: &Path, program: &str) -> Vec<PathBuf> {
        let requested = Path::new(program);
        if requested.extension().is_some() {
            return vec![directory.join(requested)];
        }
        self.path_extensions
            .iter()
            .map(|extension| directory.join(format!("{program}{extension}")))
            .collect()
    }
}

#[cfg(windows)]
fn controlled_path_extensions() -> Arc<[String]> {
    let allowed = [".exe", ".com", ".cmd", ".bat"];
    let configured = env::var("PATHEXT").unwrap_or_else(|_| allowed.join(";"));
    let mut extensions = Vec::new();
    for extension in configured.split(';') {
        let lower = extension.to_ascii_lowercase();
        if allowed.contains(&lower.as_str()) && !extensions.contains(&lower) {
            extensions.push(lower);
        }
    }
    if extensions.is_empty() {
        extensions.extend(allowed.map(str::to_owned));
    }
    extensions.into()
}

fn has_separator(program: &str) -> bool {
    program.contains('/') || program.contains('\\')
}

/// A failed candidate. `probed` distinguishes "the file is there but unusable" from "the
/// metadata probe saw nothing", which is what lets the PATH search continue without the
/// second `exists()` stat the guard used to perform. The guard keys on the probe outcome
/// rather than on `ErrorKind::NotFound` so that an unreadable PATH directory keeps being
/// skipped instead of aborting the whole search.
struct CandidateFailure {
    probed: bool,
    error: ProcessError,
}

impl CandidateFailure {
    fn missing(candidate: &Path, error: &io::Error) -> Self {
        Self {
            probed: false,
            error: ProcessError::Resolve(format!(
                "cannot resolve {}: {error}",
                candidate.display()
            )),
        }
    }

    fn rejected(error: ProcessError) -> Self {
        Self {
            probed: true,
            error,
        }
    }
}

fn resolve_candidate(
    candidate: &Path,
    canonical_parent: Option<&Path>,
) -> Result<ResolvedProgram, CandidateFailure> {
    let metadata =
        fs::metadata(candidate).map_err(|error| CandidateFailure::missing(candidate, &error))?;
    if !metadata.is_file() {
        return Err(CandidateFailure::rejected(ProcessError::Resolve(format!(
            "program is not a regular file: {}",
            candidate.display()
        ))));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(CandidateFailure::rejected(ProcessError::Resolve(format!(
                "program is not executable: {}",
                candidate.display()
            ))));
        }
    }
    let executable = fs::canonicalize(candidate).map_err(|error| {
        CandidateFailure::rejected(ProcessError::Resolve(format!(
            "cannot normalize {}: {error}",
            candidate.display()
        )))
    })?;
    let file_name = candidate.file_name().ok_or_else(|| {
        CandidateFailure::rejected(ProcessError::Resolve(format!(
            "program path has no executable name: {}",
            candidate.display()
        )))
    })?;
    let parent = candidate.parent().ok_or_else(|| {
        CandidateFailure::rejected(ProcessError::Resolve(format!(
            "program path has no parent directory: {}",
            candidate.display()
        )))
    })?;
    // `absolute` deliberately keeps the requested file name rather than the canonical one so
    // multicall proxies such as `cargo -> rustup` still launch under their invoked identity.
    let absolute = match canonical_parent {
        Some(canonical) => canonical.join(file_name),
        None => fs::canonicalize(parent)
            .map_err(|error| {
                CandidateFailure::rejected(ProcessError::Resolve(format!(
                    "cannot normalize program directory {}: {error}",
                    parent.display()
                )))
            })?
            .join(file_name),
    };
    let launcher = launcher_for(&executable).map_err(CandidateFailure::rejected)?;
    Ok(ResolvedProgram {
        absolute,
        executable,
        launcher,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Launcher {
    Native,
    #[cfg(windows)]
    CmdCompat,
}

impl Launcher {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Native => "native",
            #[cfg(windows)]
            Self::CmdCompat => "cmd-compat",
        }
    }
}

#[cfg(windows)]
pub(crate) fn launcher_for(path: &Path) -> Result<Launcher, ProcessError> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "exe" | "com" => Ok(Launcher::Native),
        "cmd" | "bat" => Ok(Launcher::CmdCompat),
        "ps1" => Err(ProcessError::Validation(
            ".ps1 requires a PowerShell launcher, which is not implemented".to_owned(),
        )),
        _ => Err(ProcessError::Resolve(format!(
            "unsupported Windows executable extension: .{extension}"
        ))),
    }
}

#[cfg(not(windows))]
pub(crate) fn launcher_for(path: &Path) -> Result<Launcher, ProcessError> {
    if path.as_os_str().is_empty() {
        return Err(ProcessError::Resolve("empty executable path".to_owned()));
    }
    Ok(Launcher::Native)
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedProgram {
    pub(crate) absolute: PathBuf,
    pub(crate) executable: PathBuf,
    pub(crate) launcher: Launcher,
}
