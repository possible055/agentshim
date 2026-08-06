use std::{
    cmp::Ordering,
    env,
    fmt::Display,
    io,
    path::{Component, Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use cap_std::{
    ambient_authority,
    fs::{Dir, File, OpenOptions},
};

#[cfg(windows)]
static VALIDATED_VOLUMES: std::sync::OnceLock<std::sync::Mutex<Vec<Vec<u16>>>> =
    std::sync::OnceLock::new();

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReadScope {
    #[default]
    Normal,
    Unrestricted,
}

impl Display for ReadScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Normal => formatter.write_str("normal"),
            Self::Unrestricted => formatter.write_str("unrestricted"),
        }
    }
}

impl FromStr for ReadScope {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "normal" => Ok(Self::Normal),
            "unrestricted" => Ok(Self::Unrestricted),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("read scope must be either `normal` or `unrestricted`, got `{value}`"),
            )),
        }
    }
}

#[derive(Debug)]
pub struct RepositoryRoot {
    path: PathBuf,
    capability: Arc<Dir>,
}

impl RepositoryRoot {
    /// Normalize, qualify, and retain a capability for one repository root.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the root cannot be canonicalized, does not meet
    /// the platform storage contract, or cannot be opened as a capability.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let input = path.as_ref();
        if !input.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "repository root must be absolute",
            ));
        }
        let path = std::fs::canonicalize(input)?;
        #[cfg(windows)]
        validate_platform_root(&path)?;
        let capability = Dir::open_ambient_dir(&path, ambient_authority())?;
        Ok(Self {
            path,
            capability: Arc::new(capability),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn capability(&self) -> &Arc<Dir> {
        &self.capability
    }

    /// Convert an absolute or root-relative input into a capability key.
    ///
    /// This is lexical admission only; callers must still use [`Self::capability`]
    /// for the actual filesystem operation.
    ///
    /// # Errors
    ///
    /// Returns a path error for NUL, ambiguous prefixes, or lexical root escape.
    pub fn resolve(&self, input: &Path) -> Result<ResolvedPath, PathError> {
        reject_nul(input)?;
        let mut key = if input.is_absolute() {
            let absolute = normalize_absolute(input)?;
            relative_from_absolute(&self.path, &absolute)?
        } else {
            normalize_relative(input)?
        };
        if key.as_os_str().is_empty() {
            key.push(".");
        }
        let absolute = self.path.join(&key);
        Ok(ResolvedPath {
            sort_key: PathSortKey::new(&key),
            slash_path: slash_path(&key),
            capability_key: key.clone(),
            key,
            absolute,
            backend: PathBackend::Repository,
        })
    }

    /// Verify that the retained root handle remains accessible.
    ///
    /// # Errors
    ///
    /// Returns the capability-relative metadata error.
    pub fn verify(&self) -> io::Result<()> {
        self.capability.metadata(".").map(|_| ())
    }
}

#[derive(Debug)]
pub struct FileAccess {
    root: Arc<RepositoryRoot>,
    scope: ReadScope,
    codex_roots: Vec<Arc<RepositoryRoot>>,
}

impl FileAccess {
    #[must_use]
    pub fn new(root: Arc<RepositoryRoot>, scope: ReadScope) -> Self {
        let codex_roots = if scope == ReadScope::Normal {
            discover_codex_roots()
        } else {
            Vec::new()
        };
        Self {
            root,
            scope,
            codex_roots,
        }
    }

    #[cfg(test)]
    fn with_codex_roots(root: Arc<RepositoryRoot>, roots: &[&Path]) -> io::Result<Self> {
        let codex_roots = roots
            .iter()
            .map(RepositoryRoot::open)
            .map(|root| root.map(Arc::new))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            root,
            scope: ReadScope::Normal,
            codex_roots,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Arc<RepositoryRoot> {
        &self.root
    }

    #[must_use]
    pub fn scope(&self) -> ReadScope {
        self.scope
    }

    /// Resolve one input through the repository capability or the explicitly enabled ambient path.
    ///
    /// # Errors
    ///
    /// Returns a path error for malformed input, repository escape in repository mode, or an
    /// unsupported ambient location.
    pub fn resolve(&self, input: &Path) -> Result<ResolvedPath, PathError> {
        match self.root.resolve(input) {
            Ok(path) => Ok(path),
            Err(PathError::OutsideRoot)
                if self.scope == ReadScope::Normal && input.is_absolute() =>
            {
                self.resolve_codex(input)
            }
            Err(PathError::OutsideRoot)
                if self.scope == ReadScope::Unrestricted && input.is_absolute() =>
            {
                Self::resolve_ambient(input)
            }
            Err(error) => Err(error),
        }
    }

    /// Resolve an ambient traversal entry relative to the request's logical root.
    ///
    /// # Errors
    ///
    /// Returns a path error when either path is malformed or the entry is outside the logical root.
    pub fn resolve_external_entry(
        &self,
        operation_root: &ResolvedPath,
        absolute: &Path,
    ) -> Result<ResolvedPath, PathError> {
        if !operation_root.is_external() {
            return self.resolve(absolute);
        }
        reject_nul(absolute)?;
        let absolute = normalize_absolute(absolute)?;
        let mut key = absolute
            .strip_prefix(operation_root.absolute())
            .map_err(|_| PathError::OutsideRoot)?
            .to_path_buf();
        if key.as_os_str().is_empty() {
            key.push(".");
        }
        if key
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return Err(PathError::OutsideRoot);
        }
        match operation_root.backend {
            PathBackend::Repository => unreachable!("repository paths are not external"),
            PathBackend::Codex(index) => {
                let capability_key = self.codex_roots[index].resolve(&absolute)?.key.clone();
                Ok(ResolvedPath::codex(absolute, key, capability_key, index))
            }
            PathBackend::Ambient => Ok(ResolvedPath::ambient(absolute, key)),
        }
    }

    pub(crate) fn resolve_traversal_entry(
        &self,
        operation_root: &ResolvedPath,
        absolute: &Path,
    ) -> Result<ResolvedPath, PathError> {
        if operation_root.is_external() {
            self.resolve_external_entry(operation_root, absolute)
        } else {
            self.resolve(absolute)
        }
    }

    /// Read metadata through the backend that admitted the path.
    ///
    /// # Errors
    ///
    /// Returns the operating-system or capability-relative metadata error.
    pub fn metadata_kind(&self, path: &ResolvedPath) -> io::Result<PathKind> {
        match path.backend {
            PathBackend::Repository => {
                let file_type = self
                    .root
                    .capability()
                    .metadata(path.capability_key())?
                    .file_type();
                Ok(PathKind::new(
                    file_type.is_file(),
                    file_type.is_dir(),
                    file_type.is_symlink(),
                ))
            }
            PathBackend::Codex(index) => self.codex_roots[index]
                .capability()
                .metadata(path.capability_key())
                .map(|metadata| metadata.file_type())
                .map(|file_type| {
                    PathKind::new(
                        file_type.is_file(),
                        file_type.is_dir(),
                        file_type.is_symlink(),
                    )
                }),
            PathBackend::Ambient => std::fs::metadata(path.absolute()).map(|metadata| {
                let file_type = metadata.file_type();
                PathKind::new(
                    file_type.is_file(),
                    file_type.is_dir(),
                    file_type.is_symlink(),
                )
            }),
        }
    }

    /// Read non-following metadata through the backend that admitted the path.
    ///
    /// # Errors
    ///
    /// Returns the operating-system or capability-relative metadata error.
    pub fn symlink_metadata_kind(&self, path: &ResolvedPath) -> io::Result<PathKind> {
        match path.backend {
            PathBackend::Repository => self
                .root
                .capability()
                .symlink_metadata(path.capability_key())
                .map(|metadata| metadata.file_type())
                .map(|file_type| {
                    PathKind::new(
                        file_type.is_file(),
                        file_type.is_dir(),
                        file_type.is_symlink(),
                    )
                }),
            PathBackend::Codex(index) => self.codex_roots[index]
                .capability()
                .symlink_metadata(path.capability_key())
                .map(|metadata| metadata.file_type())
                .map(|file_type| {
                    PathKind::new(
                        file_type.is_file(),
                        file_type.is_dir(),
                        file_type.is_symlink(),
                    )
                }),
            PathBackend::Ambient => std::fs::symlink_metadata(path.absolute()).map(|metadata| {
                let file_type = metadata.file_type();
                PathKind::new(
                    file_type.is_file(),
                    file_type.is_dir(),
                    file_type.is_symlink(),
                )
            }),
        }
    }

    /// Open a path for reading through the backend that admitted it.
    ///
    /// # Errors
    ///
    /// Returns the operating-system or capability-relative open error.
    pub fn open_read(&self, path: &ResolvedPath) -> io::Result<File> {
        if path.backend != PathBackend::Ambient {
            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK);
            }
            return match path.backend {
                PathBackend::Repository => self
                    .root
                    .capability()
                    .open_with(path.capability_key(), &options),
                PathBackend::Codex(index) => self.codex_roots[index]
                    .capability()
                    .open_with(path.capability_key(), &options),
                PathBackend::Ambient => unreachable!("ambient path handled below"),
            };
        }

        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        options.open(path.absolute()).map(File::from_std)
    }

    fn resolve_ambient(input: &Path) -> Result<ResolvedPath, PathError> {
        reject_nul(input)?;
        let absolute = normalize_absolute(input)?;
        validate_ambient_path(&absolute)?;
        let key = absolute
            .file_name()
            .map_or_else(|| PathBuf::from("."), PathBuf::from);
        Ok(ResolvedPath::ambient(absolute, key))
    }

    fn resolve_codex(&self, input: &Path) -> Result<ResolvedPath, PathError> {
        for (index, root) in self.codex_roots.iter().enumerate() {
            match root.resolve(input) {
                Ok(path) => {
                    let key = path
                        .absolute()
                        .file_name()
                        .map_or_else(|| PathBuf::from("."), PathBuf::from);
                    return Ok(ResolvedPath::codex(path.absolute, key, path.key, index));
                }
                Err(PathError::OutsideRoot) => {}
                Err(error) => return Err(error),
            }
        }
        Err(PathError::OutsideRoot)
    }
}

fn discover_codex_roots() -> Vec<Arc<RepositoryRoot>> {
    let mut candidates = Vec::new();
    if let Some(codex_home) = env::var_os("CODEX_HOME").map(PathBuf::from) {
        candidates.push(codex_home.join("skills"));
        candidates.push(codex_home.join("plugins"));
    }
    let home = if cfg!(windows) {
        env::var_os("USERPROFILE").map(PathBuf::from)
    } else {
        env::var_os("HOME").map(PathBuf::from)
    };
    if let Some(home) = home {
        for relative in [
            [".codex", "skills"],
            [".codex", "plugins"],
            [".agents", "skills"],
            [".agents", "plugins"],
        ] {
            candidates.push(home.join(relative[0]).join(relative[1]));
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
        .into_iter()
        .filter_map(|path| RepositoryRoot::open(path).ok().map(Arc::new))
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathKind {
    pub is_file: bool,
    pub is_dir: bool,
    pub is_symlink: bool,
}

impl PathKind {
    fn new(is_file: bool, is_dir: bool, is_symlink: bool) -> Self {
        Self {
            is_file,
            is_dir,
            is_symlink,
        }
    }
}
