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
        Ok(ResolvedPath::repository(absolute, key))
    }

    /// Create or truncate a repository file through the capability.
    ///
    /// [`Self::resolve`] admits a name lexically, which a symlink or junction stored inside the
    /// repository passes even when it points outside; the write is what has to be confined, and
    /// the retained directory handle is what confines it. The parent directory must already
    /// exist, so a mistyped path fails instead of scattering files through the repository.
    ///
    /// # Errors
    ///
    /// Returns the capability-relative open error, including when the path leaves the root
    /// through a link or names a directory that does not exist.
    pub fn create_truncated(&self, path: &ResolvedPath) -> io::Result<std::fs::File> {
        let key = path.capability_key();
        if let Some(parent) = key
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            && !self
                .capability
                .metadata(parent)
                .is_ok_and(|metadata| metadata.is_dir())
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("parent directory does not exist: {}", parent.display()),
            ));
        }
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        self.capability.open_with(key, &options).map(File::into_std)
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

pub(crate) struct SameParentReader<'a> {
    access: &'a FileAccess,
    backend: PathBackend,
    parent: PathBuf,
    directory: Option<Dir>,
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

    pub(crate) fn resolve_walked_entry(
        &self,
        operation_root: &ResolvedPath,
        key: &Path,
        absolute: &Path,
    ) -> Result<ResolvedPath, PathError> {
        reject_nul(key)?;
        if key.as_os_str().is_empty()
            || key
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return self.resolve_traversal_entry(operation_root, absolute);
        }

        let expected = if operation_root.is_external() {
            operation_root.absolute().join(key)
        } else {
            self.root.path().join(key)
        };
        if expected.as_os_str() != absolute.as_os_str() {
            return self.resolve_traversal_entry(operation_root, absolute);
        }

        match operation_root.backend {
            PathBackend::Repository => {
                Ok(ResolvedPath::repository(absolute.to_path_buf(), key.to_path_buf()))
            }
            PathBackend::Codex(index) => {
                let operation_key = operation_root.capability_key();
                let capability_key =
                    if operation_key.as_os_str().is_empty() || operation_key == Path::new(".") {
                        key.to_path_buf()
                    } else {
                        operation_key.join(key)
                    };
                Ok(ResolvedPath::codex(
                    absolute.to_path_buf(),
                    key.to_path_buf(),
                    capability_key,
                    index,
                ))
            }
            PathBackend::Ambient => Ok(ResolvedPath::ambient(
                absolute.to_path_buf(),
                key.to_path_buf(),
            )),
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
            let options = capability_read_options();
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

    #[cfg(all(any(test, feature = "bench-internals"), windows))]
    pub(crate) fn open_file_identity(&self, path: &ResolvedPath) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;

        if path.backend != PathBackend::Ambient {
            let options = capability_identity_options();
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
        options.read(true).access_mode(FILE_READ_ATTRIBUTES);
        options.open(path.absolute()).map(File::from_std)
    }

    #[cfg(all(any(test, feature = "bench-internals"), not(windows)))]
    pub(crate) fn open_file_identity(&self, path: &ResolvedPath) -> io::Result<File> {
        self.open_read(path)
    }

    #[cfg(any(test, feature = "bench-internals"))]
    pub(crate) fn open_read_same_parent_batch(
        &self,
        paths: &[ResolvedPath],
    ) -> io::Result<Vec<io::Result<File>>> {
        let Some(first) = paths.first() else {
            return Ok(Vec::new());
        };
        if paths.iter().any(|path| !first.has_same_parent(path)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch paths must use the same backend and parent directory",
            ));
        }
        let reader = self.open_same_parent_reader(first)?;
        Ok(paths.iter().map(|path| reader.open(path)).collect())
    }

    pub(crate) fn open_same_parent_reader(
        &self,
        first: &ResolvedPath,
    ) -> io::Result<SameParentReader<'_>> {
        let parent = batch_parent(first)?.to_path_buf();
        let directory = match first.backend {
            PathBackend::Repository => Some(self.root.capability().open_dir(batch_parent_key(
                &parent,
            ))?),
            PathBackend::Codex(index) => Some(
                self.codex_roots[index]
                    .capability()
                    .open_dir(batch_parent_key(&parent))?,
            ),
            PathBackend::Ambient => None,
        };
        Ok(SameParentReader {
            access: self,
            backend: first.backend,
            parent,
            directory,
        })
    }

    fn resolve_ambient(input: &Path) -> Result<ResolvedPath, PathError> {
        reject_nul(input)?;
        let absolute = normalize_absolute(input)?;
        #[cfg(windows)]
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

fn capability_read_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    options
}

#[cfg(all(any(test, feature = "bench-internals"), windows))]
fn capability_identity_options() -> OpenOptions {
    use cap_std::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;

    let mut options = OpenOptions::new();
    options.read(true).access_mode(FILE_READ_ATTRIBUTES);
    options
}

fn batch_parent(path: &ResolvedPath) -> io::Result<&Path> {
    let path = if path.backend == PathBackend::Ambient {
        path.absolute()
    } else {
        path.capability_key()
    };
    path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "batch path must have a parent directory",
        )
    })
}

fn batch_parent_key(parent: &Path) -> &Path {
    if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    }
}

impl SameParentReader<'_> {
    pub(crate) fn open(&self, path: &ResolvedPath) -> io::Result<File> {
        if path.backend != self.backend || batch_parent(path)? != self.parent {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch paths must use the same backend and parent directory",
            ));
        }
        let Some(directory) = &self.directory else {
            return self.access.open_read(path);
        };
        let name = path.capability_key().file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch path must name a child entry",
            )
        })?;
        directory.open_with(name, &capability_read_options())
    }

    #[cfg(any(test, feature = "bench-internals"))]
    pub(crate) fn open_identity(&self, path: &ResolvedPath) -> io::Result<File> {
        if path.backend != self.backend || batch_parent(path)? != self.parent {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch paths must use the same backend and parent directory",
            ));
        }
        let Some(directory) = &self.directory else {
            return self.access.open_file_identity(path);
        };
        let name = path.capability_key().file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch path must name a child entry",
            )
        })?;
        #[cfg(windows)]
        let options = capability_identity_options();
        #[cfg(not(windows))]
        let options = capability_read_options();
        directory.open_with(name, &options)
    }

    #[cfg(any(test, feature = "bench-internals"))]
    pub(crate) fn directory(&self) -> Option<&Dir> {
        self.directory.as_ref()
    }

    #[cfg(any(test, feature = "bench-internals"))]
    pub(crate) fn reopen_parent(&self) -> io::Result<Option<Dir>> {
        let directory = match self.backend {
            PathBackend::Repository => Some(
                self.access
                    .root
                    .capability()
                    .open_dir(batch_parent_key(&self.parent))?,
            ),
            PathBackend::Codex(index) => Some(
                self.access.codex_roots[index]
                    .capability()
                    .open_dir(batch_parent_key(&self.parent))?,
            ),
            PathBackend::Ambient => None,
        };
        Ok(directory)
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
