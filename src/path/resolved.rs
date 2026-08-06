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
    #[cfg(windows)]
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

include!("tests.rs");
