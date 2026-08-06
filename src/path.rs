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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPath {
    key: PathBuf,
    capability_key: PathBuf,
    absolute: PathBuf,
    sort_key: PathSortKey,
    slash_path: Option<String>,
    backend: PathBackend,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathBackend {
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
        self.slash_path.as_deref()
    }

    #[must_use]
    pub fn is_ambient(&self) -> bool {
        self.backend == PathBackend::Ambient
    }

    #[must_use]
    pub fn is_external(&self) -> bool {
        self.backend != PathBackend::Repository
    }

    fn capability_key(&self) -> &Path {
        &self.capability_key
    }

    fn ambient(absolute: PathBuf, key: PathBuf) -> Self {
        Self {
            sort_key: PathSortKey::new(&key),
            slash_path: slash_path(&key),
            capability_key: key.clone(),
            key,
            absolute,
            backend: PathBackend::Ambient,
        }
    }

    fn codex(absolute: PathBuf, key: PathBuf, capability_key: PathBuf, index: usize) -> Self {
        Self {
            sort_key: PathSortKey::new(&key),
            slash_path: slash_path(&key),
            key,
            capability_key,
            absolute,
            backend: PathBackend::Codex(index),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathSortKey(PlatformSortKey);

#[cfg(unix)]
type PlatformSortKey = Vec<u8>;
#[cfg(windows)]
type PlatformSortKey = Vec<u16>;
#[cfg(not(any(unix, windows)))]
type PlatformSortKey = String;

impl PathSortKey {
    fn new(path: &Path) -> Self {
        Self(platform_sort_key(path))
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
    #[error("path is not on a supported local filesystem")]
    UnsupportedLocation,
}

fn normalize_relative(path: &Path) -> Result<PathBuf, PathError> {
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

fn normalize_absolute(path: &Path) -> Result<PathBuf, PathError> {
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

#[cfg(not(windows))]
fn validate_ambient_path(_path: &Path) -> Result<(), PathError> {
    Ok(())
}

#[cfg(windows)]
fn validate_ambient_path(path: &Path) -> Result<(), PathError> {
    use std::path::Prefix;

    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return Err(PathError::UnsupportedLocation);
    };
    if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
        return Err(PathError::UnsupportedLocation);
    }
    validate_platform_root(path).map_err(|_| PathError::UnsupportedLocation)
}

#[cfg(not(windows))]
fn relative_from_absolute(root: &Path, absolute: &Path) -> Result<PathBuf, PathError> {
    absolute
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| PathError::OutsideRoot)
}

#[cfg(windows)]
fn relative_from_absolute(root: &Path, absolute: &Path) -> Result<PathBuf, PathError> {
    let root_components = root.components().collect::<Vec<_>>();
    let absolute_components = absolute.components().collect::<Vec<_>>();
    if absolute_components.len() < root_components.len()
        || !root_components
            .iter()
            .zip(&absolute_components)
            .all(|(left, right)| windows_component_eq(*left, *right))
    {
        return Err(PathError::OutsideRoot);
    }
    let mut relative = PathBuf::new();
    for component in &absolute_components[root_components.len()..] {
        match component {
            Component::Normal(segment) => relative.push(segment),
            _ => return Err(PathError::OutsideRoot),
        }
    }
    Ok(relative)
}

pub(crate) fn slash_path(path: &Path) -> Option<String> {
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

#[cfg(unix)]
fn platform_sort_key(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    let mut output = Vec::new();
    for component in path.components() {
        let Component::Normal(segment) = component else {
            continue;
        };
        if !output.is_empty() {
            output.push(b'/');
        }
        output.extend_from_slice(segment.as_bytes());
    }
    output
}

#[cfg(windows)]
fn platform_sort_key(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let mut output = Vec::new();
    for component in path.components() {
        let Component::Normal(segment) = component else {
            continue;
        };
        if !output.is_empty() {
            output.push(u16::from(b'/'));
        }
        output.extend(segment.encode_wide());
    }
    output
}

#[cfg(not(any(unix, windows)))]
fn platform_sort_key(path: &Path) -> String {
    slash_path(path).unwrap_or_default()
}

#[cfg(unix)]
fn reject_nul(path: &Path) -> Result<(), PathError> {
    use std::os::unix::ffi::OsStrExt;

    if path.as_os_str().as_bytes().contains(&0) {
        Err(PathError::Nul)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn reject_nul(path: &Path) -> Result<(), PathError> {
    use std::os::windows::ffi::OsStrExt;

    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        Err(PathError::Nul)
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn reject_nul(path: &Path) -> Result<(), PathError> {
    if path.to_string_lossy().contains('\0') {
        Err(PathError::Nul)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_component_eq(left: Component<'_>, right: Component<'_>) -> bool {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Prefix;
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};

    fn os_str_eq(left: &OsStr, right: &OsStr) -> bool {
        let left = left.encode_wide().collect::<Vec<_>>();
        let right = right.encode_wide().collect::<Vec<_>>();
        let Ok(left_len) = i32::try_from(left.len()) else {
            return false;
        };
        let Ok(right_len) = i32::try_from(right.len()) else {
            return false;
        };
        // SAFETY: Both pointers remain valid for the supplied slice lengths, and TRUE is the
        // documented value for ordinal case-insensitive comparison.
        unsafe {
            CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1)
                == CSTR_EQUAL
        }
    }

    fn prefix_eq(left: Prefix<'_>, right: Prefix<'_>) -> bool {
        match (left, right) {
            (
                Prefix::Disk(left) | Prefix::VerbatimDisk(left),
                Prefix::Disk(right) | Prefix::VerbatimDisk(right),
            ) => left.eq_ignore_ascii_case(&right),
            (
                Prefix::UNC(left_server, left_share) | Prefix::VerbatimUNC(left_server, left_share),
                Prefix::UNC(right_server, right_share)
                | Prefix::VerbatimUNC(right_server, right_share),
            ) => os_str_eq(left_server, right_server) && os_str_eq(left_share, right_share),
            (Prefix::Verbatim(left), Prefix::Verbatim(right))
            | (Prefix::DeviceNS(left), Prefix::DeviceNS(right)) => os_str_eq(left, right),
            _ => false,
        }
    }

    match (left, right) {
        (Component::Prefix(left), Component::Prefix(right)) => prefix_eq(left.kind(), right.kind()),
        (Component::RootDir, Component::RootDir)
        | (Component::CurDir, Component::CurDir)
        | (Component::ParentDir, Component::ParentDir) => true,
        (Component::Normal(left), Component::Normal(right)) => os_str_eq(left, right),
        _ => false,
    }
}

#[cfg(windows)]
fn validate_platform_root(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW,
    };
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

    validate_windows_version()?;

    let mut input = path.as_os_str().encode_wide().collect::<Vec<_>>();
    input.push(0);
    let mut volume_path = vec![0_u16; 32_768];
    let volume_capacity = u32::try_from(volume_path.len()).expect("volume path buffer fits DWORD");
    // SAFETY: Input is NUL-terminated and the output pointer refers to a writable buffer whose
    // length is supplied in UTF-16 code units.
    if unsafe { GetVolumePathNameW(input.as_ptr(), volume_path.as_mut_ptr(), volume_capacity) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let terminator = volume_path
        .iter()
        .position(|unit| *unit == 0)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "volume path was not terminated")
        })?;
    volume_path.truncate(terminator + 1);

    let validated_volumes = VALIDATED_VOLUMES.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    if validated_volumes
        .lock()
        .map_err(|_| io::Error::other("validated volume cache is unavailable"))?
        .iter()
        .any(|validated| validated == &volume_path)
    {
        return Ok(());
    }

    // SAFETY: `volume_path` is a live, NUL-terminated UTF-16 root path.
    let drive_type = unsafe { GetDriveTypeW(volume_path.as_ptr()) };
    if drive_type != DRIVE_FIXED {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows repository root must be on a local fixed drive",
        ));
    }

    let mut filesystem = [0_u16; 32];
    // SAFETY: The root input is NUL-terminated; unused optional outputs are null; the filesystem
    // buffer is writable for the supplied number of UTF-16 code units.
    let succeeded = unsafe {
        GetVolumeInformationW(
            volume_path.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            u32::try_from(filesystem.len()).expect("filesystem buffer fits DWORD"),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    let length = filesystem
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(filesystem.len());
    if !String::from_utf16_lossy(&filesystem[..length]).eq_ignore_ascii_case("NTFS") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows repository root must use NTFS",
        ));
    }
    validated_volumes
        .lock()
        .map_err(|_| io::Error::other("validated volume cache is unavailable"))?
        .push(volume_path);
    Ok(())
}

#[cfg(windows)]
fn validate_windows_version() -> io::Result<()> {
    use std::mem::size_of;
    use windows_sys::{
        Wdk::System::SystemServices::RtlGetVersion,
        Win32::System::SystemInformation::OSVERSIONINFOW,
    };

    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: u32::try_from(size_of::<OSVERSIONINFOW>())
            .map_err(|_| io::Error::other("OSVERSIONINFOW size overflow"))?,
        ..OSVERSIONINFOW::default()
    };
    if unsafe { RtlGetVersion(&raw mut version) } < 0 {
        return Err(io::Error::other("RtlGetVersion failed"));
    }
    if version.dwMajorVersion != 10 || version.dwBuildNumber < 22_621 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "codexshim requires Windows 11 build 22621 or newer",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use super::{FileAccess, PathError, ReadScope, RepositoryRoot};

    #[test]
    fn read_scope_parsing_is_explicit_and_fail_closed() {
        assert_eq!(
            "normal".parse::<ReadScope>().expect("normal"),
            ReadScope::Normal
        );
        assert_eq!(
            "unrestricted".parse::<ReadScope>().expect("unrestricted"),
            ReadScope::Unrestricted
        );
        assert!("all".parse::<ReadScope>().is_err());
    }

    #[test]
    fn normal_rejects_unmanaged_paths_and_unrestricted_admits_them() {
        let fixture = tempfile::tempdir().expect("root fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::write(outside.path().join("outside.txt"), "outside").expect("outside file");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let normal = FileAccess::new(Arc::clone(&root), ReadScope::Normal);
        let unrestricted = FileAccess::new(root, ReadScope::Unrestricted);
        let outside_file = outside.path().join("outside.txt");

        assert_eq!(
            normal.resolve(&outside_file).unwrap_err(),
            PathError::OutsideRoot
        );
        let resolved = unrestricted.resolve(&outside_file).expect("ambient path");
        assert!(resolved.is_ambient());
        assert_eq!(resolved.absolute(), outside_file);
        assert_eq!(resolved.key(), Path::new("outside.txt"));
        assert_eq!(
            unrestricted.resolve(Path::new("../outside")).unwrap_err(),
            PathError::ParentEscape
        );
    }

    #[test]
    fn normal_admits_only_configured_codex_roots() {
        let repository = tempfile::tempdir().expect("repository fixture");
        let codex = tempfile::tempdir().expect("codex fixture");
        let unmanaged = tempfile::tempdir().expect("unmanaged fixture");
        let skills = codex.path().join("skills");
        fs::create_dir_all(skills.join("example")).expect("skill directory");
        fs::write(skills.join("example/SKILL.md"), "instructions").expect("skill file");
        fs::write(unmanaged.path().join("secret.txt"), "secret").expect("unmanaged file");
        let access = FileAccess::with_codex_roots(
            Arc::new(RepositoryRoot::open(repository.path()).expect("root")),
            &[skills.as_path()],
        )
        .expect("normal access");

        let skill_root = access.resolve(&skills).expect("skill root");
        let skill = access
            .resolve(&skills.join("example/SKILL.md"))
            .expect("skill file");
        assert!(skill.is_external());
        assert!(!skill.is_ambient());
        assert!(access.metadata_kind(&skill).expect("metadata").is_file);
        assert_eq!(
            access
                .resolve_external_entry(&skill_root, skill.absolute())
                .expect("skill entry")
                .key(),
            Path::new("example/SKILL.md")
        );
        assert_eq!(
            access
                .resolve(&unmanaged.path().join("secret.txt"))
                .unwrap_err(),
            PathError::OutsideRoot
        );
    }

    #[cfg(windows)]
    #[test]
    fn unrestricted_scope_rejects_non_disk_windows_prefixes() {
        let fixture = tempfile::tempdir().expect("root fixture");
        let access = FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Unrestricted,
        );
        for path in [r"\\server\share\file.rs", r"\\.\PhysicalDrive0"] {
            assert_eq!(
                access.resolve(Path::new(path)).unwrap_err(),
                PathError::UnsupportedLocation
            );
        }
        assert_eq!(
            access.resolve(Path::new(r"C:relative.rs")).unwrap_err(),
            PathError::AmbiguousPrefix
        );
    }

    #[test]
    fn resolves_relative_and_root_absolute_paths() {
        let fixture = tempfile::tempdir().expect("create fixture");
        fs::create_dir(fixture.path().join("src")).expect("create src");
        let root = RepositoryRoot::open(fixture.path()).expect("open root");

        let relative = root.resolve(Path::new("src/./lib.rs")).expect("relative");
        let absolute = root
            .resolve(&root.path().join("src/lib.rs"))
            .expect("absolute");
        assert_eq!(relative.key(), Path::new("src/lib.rs"));
        assert_eq!(relative, absolute);
        assert_eq!(relative.slash_path(), Some("src/lib.rs"));
    }

    #[test]
    fn rejects_parent_and_absolute_escape() {
        let fixture = tempfile::tempdir().expect("create fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("open root");
        assert_eq!(
            root.resolve(Path::new("../outside")).unwrap_err(),
            PathError::ParentEscape
        );
        let outside = fixture
            .path()
            .parent()
            .expect("fixture parent")
            .join("outside");
        assert_eq!(root.resolve(&outside).unwrap_err(), PathError::OutsideRoot);
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_relative_and_case_rules_are_explicit() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("open root");
        assert_eq!(
            root.resolve(Path::new(r"C:ambiguous"))
                .expect_err("drive-relative path"),
            PathError::AmbiguousPrefix
        );

        let absolute = root.path().join("CaseSensitiveName.rs");
        let folded = absolute.to_string_lossy().to_ascii_uppercase();
        let resolved = root
            .resolve(Path::new(&folded))
            .expect("Windows absolute comparison is case-insensitive");
        assert_eq!(resolved.slash_path(), Some("CASESENSITIVENAME.RS"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_standard_and_verbatim_prefixes_are_equivalent() {
        use std::path::PathBuf;

        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("open root");
        let canonical = root.path().to_string_lossy();
        let standard = canonical
            .strip_prefix(r"\\?\")
            .expect("canonical drive path uses a verbatim prefix");
        let resolved = root
            .resolve(&PathBuf::from(standard).join("CaseSensitiveName.rs"))
            .expect("standard drive path resolves under verbatim root");
        assert_eq!(resolved.slash_path(), Some("CaseSensitiveName.rs"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefix_equivalence_is_narrow() {
        use super::windows_component_eq;

        fn prefix(path: &Path) -> std::path::Component<'_> {
            path.components().next().expect("path prefix")
        }

        assert!(windows_component_eq(
            prefix(Path::new(r"C:\repo")),
            prefix(Path::new(r"\\?\c:\repo")),
        ));
        assert!(windows_component_eq(
            prefix(Path::new(r"\\server\share\repo")),
            prefix(Path::new(r"\\?\UNC\SERVER\SHARE\repo")),
        ));
        assert!(!windows_component_eq(
            prefix(Path::new(r"C:\repo")),
            prefix(Path::new(r"D:\repo")),
        ));
        assert!(!windows_component_eq(
            prefix(Path::new(r"\\server\share\repo")),
            prefix(Path::new(r"\\?\UNC\server\other\repo")),
        ));
        assert!(!windows_component_eq(
            prefix(Path::new(r"\\.\COM1")),
            prefix(Path::new(r"\\?\COM1")),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_sort_keys_are_lossless_and_not_model_visible() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let fixture = tempfile::tempdir().expect("create fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("open root");
        let path = std::path::PathBuf::from(OsString::from_vec(vec![b'a', 0xFF]));
        let resolved = root.resolve(&path).expect("resolve raw path");
        assert_eq!(resolved.slash_path(), None);
        assert!(resolved.sort_key() > root.resolve(Path::new("a")).expect("a").sort_key());
    }
}
