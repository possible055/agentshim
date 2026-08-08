use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[cfg(windows)]
use std::{
    io::{Read, Write},
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    path::RepositoryRoot,
    tools::ToolOutput,
};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_STDIN_BYTES: usize = 1024 * 1024;
const PROCESS_MEMORY_BYTES: usize = 2 * 1024 * 1024;
const CAPTURE_HEAD_BYTES: usize = 16 * 1024;
const CAPTURE_TAIL_BYTES: usize = 16 * 1024;
const DRAIN_CHUNK_BYTES: usize = 64 * 1024;
const DIAGNOSTIC_PATH_BYTES: usize = 2 * 1024;
const DIAGNOSTIC_PATH_MARKER: &str = "...[path truncated]...";
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_millis(250);
const CLEANUP_DEADLINE: Duration = Duration::from_secs(5);

const ENVIRONMENT_DEFAULTS: [(&str, &str); 6] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_PAGER", "cat"),
    ("PAGER", "cat"),
    ("CARGO_TERM_COLOR", "never"),
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessRequest {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub unset_env: Vec<String>,
    pub stdin: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProcessStreamSummary {
    #[serde(rename = "total_bytes")]
    pub total: usize,
    #[serde(rename = "shown_bytes")]
    pub shown: usize,
    #[serde(rename = "omitted_bytes")]
    pub omitted: usize,
    #[serde(rename = "invalid_utf8_bytes")]
    pub invalid_utf8: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProcessTimeoutDetails {
    pub timeout_ms: u64,
    pub program: String,
    pub cwd: String,
    pub launcher: String,
    pub duration_ms: u64,
    pub stdout: ProcessStreamSummary,
    pub stderr: ProcessStreamSummary,
    pub termination_outcome: &'static str,
}

struct TimeoutRender {
    text: String,
    details: ProcessTimeoutDetails,
}

impl std::ops::Deref for TimeoutRender {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

impl ProcessRequest {
    /// Validate all scalar and environment constraints before process admission.
    ///
    /// # Errors
    ///
    /// Returns a validation error for malformed, conflicting, or oversized input.
    pub fn validate(&self) -> Result<(), ProcessError> {
        if self.program.is_empty() {
            return Err(ProcessError::Validation(
                "program must not be empty".to_owned(),
            ));
        }
        if contains_nul(&self.program)
            || self.args.iter().any(|arg| contains_nul(arg))
            || self.cwd.as_deref().is_some_and(contains_nul)
            || self.stdin.as_deref().is_some_and(contains_nul)
        {
            return Err(ProcessError::Validation(
                "program, args, cwd, and stdin must not contain NUL".to_owned(),
            ));
        }
        if self
            .stdin
            .as_ref()
            .is_some_and(|stdin| stdin.len() > MAX_STDIN_BYTES)
        {
            return Err(ProcessError::Validation(
                "stdin must not exceed 1048576 UTF-8 bytes".to_owned(),
            ));
        }
        if !(1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms()) {
            return Err(ProcessError::Validation(
                "timeout_ms must be from 1 to 300000".to_owned(),
            ));
        }

        let mut overrides: Vec<String> = Vec::new();
        for (key, value) in &self.env {
            validate_environment(key, value)?;
            if overrides
                .iter()
                .any(|existing| environment_keys_equal(existing, key))
            {
                return Err(ProcessError::Validation(format!(
                    "env contains duplicate key {key:?} under platform comparison rules"
                )));
            }
            overrides.push(key.clone());
        }
        let mut removals: Vec<String> = Vec::new();
        for key in &self.unset_env {
            validate_environment(key, "")?;
            if removals
                .iter()
                .any(|existing| environment_keys_equal(existing, key))
            {
                return Err(ProcessError::Validation(format!(
                    "unset_env contains duplicate key {key:?}"
                )));
            }
            if overrides
                .iter()
                .any(|existing| environment_keys_equal(existing, key))
            {
                return Err(ProcessError::Validation(format!(
                    "environment key {key:?} occurs in both env and unset_env"
                )));
            }
            removals.push(key.clone());
        }
        Ok(())
    }

    #[must_use]
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)
    }

    #[must_use]
    pub fn memory_charge(&self) -> usize {
        let strings = self
            .args
            .iter()
            .map(String::len)
            .chain(self.env.iter().map(|(key, value)| key.len() + value.len()))
            .chain(self.unset_env.iter().map(String::len))
            .sum::<usize>();
        PROCESS_MEMORY_BYTES
            .saturating_add(self.program.len())
            .saturating_add(self.cwd.as_deref().map_or(0, str::len))
            .saturating_add(self.stdin.as_deref().map_or(0, str::len))
            .saturating_add(strings)
    }
}

fn contains_nul(value: &str) -> bool {
    value.contains('\0')
}

fn validate_environment(key: &str, value: &str) -> Result<(), ProcessError> {
    if key.is_empty() || key.contains('=') || contains_nul(key) || contains_nul(value) {
        return Err(ProcessError::Validation(format!(
            "invalid environment key or value for {key:?}"
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn environment_keys_equal(left: &str, right: &str) -> bool {
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};

    let left = left.encode_utf16().collect::<Vec<_>>();
    let right = right.encode_utf16().collect::<Vec<_>>();
    let left_length = i32::try_from(left.len()).unwrap_or(i32::MAX);
    let right_length = i32::try_from(right.len()).unwrap_or(i32::MAX);
    unsafe {
        CompareStringOrdinal(left.as_ptr(), left_length, right.as_ptr(), right_length, 1)
            == CSTR_EQUAL
    }
}

#[cfg(not(windows))]
fn environment_keys_equal(left: &str, right: &str) -> bool {
    left == right
}

#[derive(Clone, Debug)]
pub struct ProcessResolver {
    search_path: Arc<[PathBuf]>,
    #[cfg(windows)]
    path_extensions: Arc<[String]>,
}

impl ProcessResolver {
    #[must_use]
    pub fn capture() -> Self {
        let search_path = env::var_os("PATH")
            .map(|path| {
                env::split_paths(&path)
                    .filter(|entry| !entry.as_os_str().is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .into();
        #[cfg(windows)]
        let path_extensions = controlled_path_extensions();
        Self {
            search_path,
            #[cfg(windows)]
            path_extensions,
        }
    }

    #[cfg(all(test, unix))]
    fn for_tests(search_path: Vec<PathBuf>) -> Self {
        Self {
            search_path: search_path.into(),
            #[cfg(windows)]
            path_extensions: vec![".exe".to_owned(), ".com".to_owned()].into(),
        }
    }

    fn resolve(&self, program: &str, cwd: &Path) -> Result<ResolvedProgram, ProcessError> {
        let requested = Path::new(program);
        if requested.is_absolute() {
            return resolve_candidate(requested);
        }
        if has_separator(program) {
            return resolve_candidate(&cwd.join(requested));
        }
        for directory in self.search_path.iter() {
            let directory = if directory.is_absolute() {
                directory.clone()
            } else {
                cwd.join(directory)
            };
            #[cfg(not(windows))]
            let candidates = Self::candidates(&directory, program);
            #[cfg(windows)]
            let candidates = self.candidates(&directory, program);
            for candidate in candidates {
                match resolve_candidate(&candidate) {
                    Ok(resolved) => return Ok(resolved),
                    Err(error) if candidate.exists() => return Err(error),
                    Err(_) => {}
                }
            }
        }
        Err(ProcessError::Resolve(format!(
            "program {program:?} was not found in the captured PATH"
        )))
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

fn resolve_candidate(candidate: &Path) -> Result<ResolvedProgram, ProcessError> {
    let metadata = fs::metadata(candidate).map_err(|error| {
        ProcessError::Resolve(format!("cannot resolve {}: {error}", candidate.display()))
    })?;
    if !metadata.is_file() {
        return Err(ProcessError::Resolve(format!(
            "program is not a regular file: {}",
            candidate.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ProcessError::Resolve(format!(
                "program is not executable: {}",
                candidate.display()
            )));
        }
    }
    let executable = fs::canonicalize(candidate).map_err(|error| {
        ProcessError::Resolve(format!("cannot normalize {}: {error}", candidate.display()))
    })?;
    let file_name = candidate.file_name().ok_or_else(|| {
        ProcessError::Resolve(format!(
            "program path has no executable name: {}",
            candidate.display()
        ))
    })?;
    let parent = candidate.parent().ok_or_else(|| {
        ProcessError::Resolve(format!(
            "program path has no parent directory: {}",
            candidate.display()
        ))
    })?;
    let absolute = fs::canonicalize(parent)
        .map_err(|error| {
            ProcessError::Resolve(format!(
                "cannot normalize program directory {}: {error}",
                parent.display()
            ))
        })?
        .join(file_name);
    let launcher = launcher_for(&executable)?;
    Ok(ResolvedProgram {
        absolute,
        executable,
        launcher,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Launcher {
    Native,
    #[cfg(windows)]
    CmdCompat,
}

impl Launcher {
    fn label(self) -> &'static str {
        match self {
            Self::Native => "native",
            #[cfg(windows)]
            Self::CmdCompat => "cmd-compat",
        }
    }
}

#[cfg(windows)]
fn launcher_for(path: &Path) -> Result<Launcher, ProcessError> {
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
fn launcher_for(path: &Path) -> Result<Launcher, ProcessError> {
    if path.as_os_str().is_empty() {
        return Err(ProcessError::Resolve("empty executable path".to_owned()));
    }
    Ok(Launcher::Native)
}

#[derive(Clone, Debug)]
struct ResolvedProgram {
    absolute: PathBuf,
    executable: PathBuf,
    launcher: Launcher,
}
