use std::{
    cmp::Ordering,
    path::{Component, Path, PathBuf},
};

use super::access::batch_parent;

#[derive(Clone, Debug)]
pub struct ResolvedPath {
    pub key: PathBuf,
    capability_key: Option<PathBuf>,
    pub absolute: PathBuf,
    sort_key: PathSortKey,
    slash_path: std::sync::OnceLock<Option<String>>,
    pub backend: PathBackend,
}

impl PartialEq for ResolvedPath {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.capability_key == other.capability_key
            && self.absolute == other.absolute
            && self.sort_key == other.sort_key
            && self.backend == other.backend
    }
}

impl Eq for ResolvedPath {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathBackend {
    Repository,
    Codex(usize),
    Ambient,
}

impl ResolvedPath {
    #[must_use]
    pub fn key(&self) -> &Path {
        &self.key
    }

    #[must_use]
    pub fn absolute(&self) -> &Path {
        &self.absolute
    }

    #[must_use]
    pub fn sort_key(&self) -> &PathSortKey {
        &self.sort_key
    }

    #[must_use]
    pub fn slash_path(&self) -> Option<&str> {
        self.slash_path
            .get_or_init(|| slash_path(&self.key))
            .as_deref()
    }

    #[must_use]
    pub fn is_ambient(&self) -> bool {
        self.backend == PathBackend::Ambient
    }

    #[must_use]
    pub fn is_external(&self) -> bool {
        self.backend != PathBackend::Repository
    }

    pub fn has_same_parent(&self, other: &Self) -> bool {
        self.backend == other.backend
            && batch_parent(self).ok().is_some()
            && batch_parent(self).ok() == batch_parent(other).ok()
    }

    pub fn memory_components(&self) -> ResolvedPathMemory {
        ResolvedPathMemory {
            key_bytes: self.key.as_os_str().len(),
            key_capacity: self.key.capacity(),
            capability_key_bytes: self
                .capability_key
                .as_ref()
                .map_or(0, |key| key.as_os_str().len()),
            capability_key_capacity: self.capability_key.as_ref().map_or(0, PathBuf::capacity),
            absolute_bytes: self.absolute.as_os_str().len(),
            absolute_capacity: self.absolute.capacity(),
            sort_key_bytes: self.sort_key.byte_len(),
            sort_key_capacity: self.sort_key.capacity_bytes(),
            slash_path_bytes: self
                .slash_path
                .get()
                .and_then(Option::as_ref)
                .map_or(0, String::len),
            slash_path_capacity: self
                .slash_path
                .get()
                .and_then(Option::as_ref)
                .map_or(0, String::capacity),
        }
    }

    pub fn capability_key(&self) -> &Path {
        self.capability_key.as_deref().unwrap_or(&self.key)
    }

    pub fn repository(absolute: PathBuf, key: PathBuf) -> Self {
        Self {
            sort_key: PathSortKey::new(&key),
            slash_path: std::sync::OnceLock::new(),
            capability_key: None,
            key,
            absolute,
            backend: PathBackend::Repository,
        }
    }

    pub fn ambient(absolute: PathBuf, key: PathBuf) -> Self {
        Self {
            sort_key: PathSortKey::new(&key),
            slash_path: std::sync::OnceLock::new(),
            capability_key: None,
            key,
            absolute,
            backend: PathBackend::Ambient,
        }
    }

    pub fn codex(absolute: PathBuf, key: PathBuf, capability_key: PathBuf, index: usize) -> Self {
        Self {
            sort_key: PathSortKey::new(&key),
            slash_path: std::sync::OnceLock::new(),
            key,
            capability_key: Some(capability_key),
            absolute,
            backend: PathBackend::Codex(index),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ResolvedPathMemory {
    pub key_bytes: usize,
    pub key_capacity: usize,
    pub capability_key_bytes: usize,
    pub capability_key_capacity: usize,
    pub absolute_bytes: usize,
    pub absolute_capacity: usize,
    pub sort_key_bytes: usize,
    pub sort_key_capacity: usize,
    pub slash_path_bytes: usize,
    pub slash_path_capacity: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathSortKey(crate::platform::path::SortKey);

impl PathSortKey {
    pub fn new(path: &Path) -> Self {
        Self(crate::platform::path::sort_key(path))
    }

    fn byte_len(&self) -> usize {
        crate::platform::path::sort_key_byte_len(&self.0)
    }

    pub fn capacity_bytes(&self) -> usize {
        crate::platform::path::sort_key_capacity_bytes(&self.0)
    }
}

impl Ord for PathSortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for PathSortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PathError {
    #[error("path contains NUL")]
    Nul,
    #[error("path is outside the repository root")]
    OutsideRoot,
    #[error(
        "path has an absolute, rooted, or drive-relative prefix where a relative path is required"
    )]
    AmbiguousPrefix,
    #[error("path escapes the repository root through '..'")]
    ParentEscape,
    #[cfg(windows)]
    #[error("path is not on a supported local filesystem")]
    UnsupportedLocation,
}

pub fn normalize_relative(path: &Path) -> Result<PathBuf, PathError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => normalized.push(segment),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(PathError::ParentEscape);
                }
            }
            Component::Prefix(_) | Component::RootDir => return Err(PathError::AmbiguousPrefix),
        }
    }
    Ok(normalized)
}

pub fn normalize_absolute(path: &Path) -> Result<PathBuf, PathError> {
    if !path.is_absolute() {
        return Err(PathError::AmbiguousPrefix);
    }
    let mut normalized = PathBuf::new();
    let mut normal_components = 0_usize;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::Normal(segment) => {
                normalized.push(segment);
                normal_components += 1;
            }
            Component::ParentDir => {
                if normal_components == 0 {
                    return Err(PathError::ParentEscape);
                }
                normalized.pop();
                normal_components -= 1;
            }
        }
    }
    Ok(normalized)
}

pub fn slash_path(path: &Path) -> Option<String> {
    let mut output = String::new();
    for component in path.components() {
        let Component::Normal(segment) = component else {
            continue;
        };
        if !output.is_empty() {
            output.push('/');
        }
        output.push_str(segment.to_str()?);
    }
    Some(output)
}

pub fn display_path(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('\\', "/");
    if let Some(rest) = value.strip_prefix("//?/UNC/") {
        value = format!("//{rest}");
    } else if let Some(rest) = value.strip_prefix("//?/") {
        value = rest.to_string();
    }
    value
}
