use std::path::{Component, Path, PathBuf};

use crate::path::PathError;

#[cfg(unix)]
pub type SortKey = Vec<u8>;
#[cfg(windows)]
pub type SortKey = Vec<u16>;

#[cfg(windows)]
static VALIDATED_VOLUMES: std::sync::OnceLock<std::sync::Mutex<Vec<Vec<u16>>>> =
    std::sync::OnceLock::new();

#[cfg(unix)]
pub fn sort_key(path: &Path) -> SortKey {
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
pub fn sort_key(path: &Path) -> SortKey {
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

pub fn sort_key_byte_len(key: &SortKey) -> usize {
    #[cfg(unix)]
    {
        key.len()
    }
    #[cfg(windows)]
    {
        key.len().saturating_mul(std::mem::size_of::<u16>())
    }
}

pub fn sort_key_capacity_bytes(key: &SortKey) -> usize {
    #[cfg(unix)]
    {
        key.capacity()
    }
    #[cfg(windows)]
    {
        key.capacity().saturating_mul(std::mem::size_of::<u16>())
    }
}

#[cfg(unix)]
pub fn reject_nul(path: &Path) -> Result<(), PathError> {
    use std::os::unix::ffi::OsStrExt;

    if path.as_os_str().as_bytes().contains(&0) {
        Err(PathError::Nul)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
pub fn reject_nul(path: &Path) -> Result<(), PathError> {
    use std::os::windows::ffi::OsStrExt;

    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        Err(PathError::Nul)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
pub fn relative_from_absolute(root: &Path, absolute: &Path) -> Result<PathBuf, PathError> {
    absolute
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| PathError::OutsideRoot)
}

#[cfg(windows)]
pub fn relative_from_absolute(root: &Path, absolute: &Path) -> Result<PathBuf, PathError> {
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

#[cfg(windows)]
pub fn validate_ambient_path(path: &Path) -> Result<(), PathError> {
    use std::path::Prefix;

    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return Err(PathError::UnsupportedLocation);
    };
    if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
        return Err(PathError::UnsupportedLocation);
    }
    validate_root(path).map_err(|_| PathError::UnsupportedLocation)
}

#[cfg(windows)]
pub fn windows_component_eq(left: Component<'_>, right: Component<'_>) -> bool {
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
        // Safety: both buffers outlive the call with lengths matching their
        // slices; `CompareStringOrdinal` only reads them and is otherwise
        // side-effect free.
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
pub fn validate_root(path: &Path) -> std::io::Result<()> {
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
    // Safety: `input` is NUL-terminated and `volume_path`'s length matches the
    // capacity passed; the call only writes inside that buffer.
    if unsafe { GetVolumePathNameW(input.as_ptr(), volume_path.as_mut_ptr(), volume_capacity) } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let terminator = volume_path
        .iter()
        .position(|unit| *unit == 0)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "volume path was not terminated",
            )
        })?;
    volume_path.truncate(terminator + 1);

    let validated_volumes = VALIDATED_VOLUMES.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    if validated_volumes
        .lock()
        .map_err(|_| std::io::Error::other("validated volume cache is unavailable"))?
        .iter()
        .any(|validated| validated == &volume_path)
    {
        return Ok(());
    }

    // Safety: `volume_path` is NUL-terminated and outlives the read-only call.
    let drive_type = unsafe { GetDriveTypeW(volume_path.as_ptr()) };
    if drive_type != DRIVE_FIXED {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Windows repository root must be on a local fixed drive",
        ));
    }

    let mut filesystem = [0_u16; 32];
    // Safety: `volume_path` is NUL-terminated; the null_mut() parameters are
    // explicitly optional per the API contract, and the `filesystem` pointer
    // and length pair matches the buffer.
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
        return Err(std::io::Error::last_os_error());
    }
    let length = filesystem
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(filesystem.len());
    let filesystem = String::from_utf16_lossy(&filesystem[..length]);
    if !is_supported_windows_filesystem(&filesystem) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Windows repository root must use NTFS or ReFS",
        ));
    }
    validated_volumes
        .lock()
        .map_err(|_| std::io::Error::other("validated volume cache is unavailable"))?
        .push(volume_path);
    Ok(())
}

#[cfg(windows)]
fn is_supported_windows_filesystem(filesystem: &str) -> bool {
    filesystem.eq_ignore_ascii_case("NTFS") || filesystem.eq_ignore_ascii_case("ReFS")
}

#[cfg(windows)]
fn validate_windows_version() -> std::io::Result<()> {
    use std::mem::size_of;
    use windows_sys::{
        Wdk::System::SystemServices::RtlGetVersion,
        Win32::System::SystemInformation::OSVERSIONINFOW,
    };

    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: u32::try_from(size_of::<OSVERSIONINFOW>())
            .map_err(|_| std::io::Error::other("OSVERSIONINFOW size overflow"))?,
        ..OSVERSIONINFOW::default()
    };
    // Safety: the struct is fully initialized with `dwOSVersionInfoSize`
    // matching its size; `RtlGetVersion` only writes into it.
    if unsafe { RtlGetVersion(&raw mut version) } < 0 {
        return Err(std::io::Error::other("RtlGetVersion failed"));
    }
    if version.dwMajorVersion < 10 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "agentshim requires Windows 10 or newer",
        ));
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::is_supported_windows_filesystem;

    #[test]
    fn supports_ntfs_and_refs_only() {
        for filesystem in ["NTFS", "ntfs", "ReFS", "refs"] {
            assert!(is_supported_windows_filesystem(filesystem));
        }
        for filesystem in ["FAT32", "exFAT", ""] {
            assert!(!is_supported_windows_filesystem(filesystem));
        }
    }
}
