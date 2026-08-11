use std::io;

use cap_std::fs::File;

#[cfg(test)]
pub(super) use super::prepared::has_binary_magic;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileFingerprint {
    pub(crate) regular: bool,
    platform: PlatformFingerprint,
}

#[cfg(feature = "bench-internals")]
#[derive(Clone, Copy, Debug, Default)]
pub struct FingerprintMetrics {
    pub file_id_calls: usize,
    pub file_id_ns: u64,
    pub standard_calls: usize,
    pub standard_ns: u64,
    pub basic_calls: usize,
    pub basic_ns: u64,
}

#[cfg(all(feature = "bench-internals", windows))]
static FINGERPRINT_FILE_ID_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(all(feature = "bench-internals", windows))]
static FINGERPRINT_FILE_ID_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(all(feature = "bench-internals", windows))]
static FINGERPRINT_STANDARD_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(all(feature = "bench-internals", windows))]
static FINGERPRINT_STANDARD_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(all(feature = "bench-internals", windows))]
static FINGERPRINT_BASIC_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(all(feature = "bench-internals", windows))]
static FINGERPRINT_BASIC_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "bench-internals")]
pub fn reset_fingerprint_metrics() {
    #[cfg(windows)]
    {
        for counter in [
            &FINGERPRINT_FILE_ID_CALLS,
            &FINGERPRINT_STANDARD_CALLS,
            &FINGERPRINT_BASIC_CALLS,
        ] {
            counter.store(0, std::sync::atomic::Ordering::Relaxed);
        }
        for counter in [
            &FINGERPRINT_FILE_ID_NS,
            &FINGERPRINT_STANDARD_NS,
            &FINGERPRINT_BASIC_NS,
        ] {
            counter.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(all(feature = "bench-internals", windows))]
#[must_use]
pub fn fingerprint_metrics() -> FingerprintMetrics {
    FingerprintMetrics {
        file_id_calls: FINGERPRINT_FILE_ID_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        file_id_ns: FINGERPRINT_FILE_ID_NS.load(std::sync::atomic::Ordering::Relaxed),
        standard_calls: FINGERPRINT_STANDARD_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        standard_ns: FINGERPRINT_STANDARD_NS.load(std::sync::atomic::Ordering::Relaxed),
        basic_calls: FINGERPRINT_BASIC_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        basic_ns: FINGERPRINT_BASIC_NS.load(std::sync::atomic::Ordering::Relaxed),
    }
}

#[cfg(all(feature = "bench-internals", not(windows)))]
#[must_use]
pub fn fingerprint_metrics() -> FingerprintMetrics {
    FingerprintMetrics::default()
}

#[cfg(all(feature = "bench-internals", windows))]
fn record_fingerprint_query(class: i32, elapsed: std::time::Duration) {
    use windows_sys::Win32::Storage::FileSystem::{FileBasicInfo, FileIdInfo, FileStandardInfo};

    let elapsed = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    let (calls, nanoseconds) = if class == FileIdInfo {
        (&FINGERPRINT_FILE_ID_CALLS, &FINGERPRINT_FILE_ID_NS)
    } else if class == FileStandardInfo {
        (&FINGERPRINT_STANDARD_CALLS, &FINGERPRINT_STANDARD_NS)
    } else if class == FileBasicInfo {
        (&FINGERPRINT_BASIC_CALLS, &FINGERPRINT_BASIC_NS)
    } else {
        return;
    };
    calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _ = nanoseconds.fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |current| Some(current.saturating_add(elapsed)),
    );
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformFingerprint {
    device: u64,
    inode: u64,
    nlink: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(windows)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformFingerprint {
    volume: u64,
    file_id: [u8; 16],
    length: i64,
    last_write_time: i64,
    change_time: i64,
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformFingerprint {
    length: u64,
    modified: Option<std::time::SystemTime>,
}

/// FNV-1a. Chosen over `DefaultHasher` because the token has to mean the same thing in
/// a later process, and `DefaultHasher`'s output is explicitly not guaranteed stable
/// across toolchain releases. This is a mix-up guard, not a security digest.
fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl FileFingerprint {
    /// Opaque token identifying this exact source version.
    ///
    /// Continuation requests carry it back so a source replaced between rounds is
    /// rejected instead of silently stitching two different documents together. It is
    /// derived from the same fingerprint the retry logic already trusts, and is
    /// deliberately a hash so identity and timestamps are not exposed to the caller.
    pub(crate) fn source_id(&self) -> String {
        let mut material = Vec::with_capacity(64);
        material.push(u8::from(self.regular));
        #[cfg(unix)]
        {
            material.extend_from_slice(&self.platform.device.to_le_bytes());
            material.extend_from_slice(&self.platform.inode.to_le_bytes());
            material.extend_from_slice(&self.platform.length.to_le_bytes());
            material.extend_from_slice(&self.platform.modified_seconds.to_le_bytes());
            material.extend_from_slice(&self.platform.modified_nanoseconds.to_le_bytes());
        }
        #[cfg(windows)]
        {
            material.extend_from_slice(&self.platform.volume.to_le_bytes());
            material.extend_from_slice(&self.platform.file_id);
            material.extend_from_slice(&self.platform.length.to_le_bytes());
            material.extend_from_slice(&self.platform.last_write_time.to_le_bytes());
            material.extend_from_slice(&self.platform.change_time.to_le_bytes());
        }
        #[cfg(not(any(unix, windows)))]
        {
            material.extend_from_slice(&self.platform.length.to_le_bytes());
            if let Some(modified) = self.platform.modified
                && let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH)
            {
                material.extend_from_slice(&since.as_nanos().to_le_bytes());
            }
        }
        format!("{:016x}", stable_hash(&material))
    }

    #[must_use]
    pub(crate) fn length(&self) -> u64 {
        #[cfg(windows)]
        {
            u64::try_from(self.platform.length).unwrap_or(0)
        }
        #[cfg(not(windows))]
        {
            self.platform.length
        }
    }

    #[cfg(any(test, feature = "bench-internals"))]
    pub(crate) fn from_dir(directory: &cap_std::fs::Dir) -> io::Result<Self> {
        let file = File::from_std(directory.try_clone()?.into_std_file());
        Self::from_file(&file)
    }

    #[cfg(any(test, feature = "bench-internals"))]
    pub(crate) fn same_file(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.platform.device == other.platform.device
                && self.platform.inode == other.platform.inode
        }
        #[cfg(windows)]
        {
            self.platform.volume == other.platform.volume
                && self.platform.file_id == other.platform.file_id
        }
        #[cfg(not(any(unix, windows)))]
        {
            self == other
        }
    }

    #[cfg(windows)]
    pub(crate) fn matches_current_state(&self, file: &File) -> io::Result<bool> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FILE_STANDARD_INFO, FileBasicInfo, FileStandardInfo,
        };

        let handle = file.as_raw_handle();
        let standard: FILE_STANDARD_INFO = query_file_information(handle, FileStandardInfo)?;
        let basic: FILE_BASIC_INFO = query_file_information(handle, FileBasicInfo)?;
        Ok(self.regular != standard.Directory
            && self.platform.length == standard.EndOfFile
            && self.platform.last_write_time == basic.LastWriteTime
            && self.platform.change_time == basic.ChangeTime)
    }

    #[cfg(unix)]
    pub(crate) fn matches_current_state(&self, file: &File) -> io::Result<bool> {
        let current = Self::from_file(file)?;
        let state_unchanged = self.regular == current.regular
            && self.platform.length == current.platform.length
            && self.platform.modified_seconds == current.platform.modified_seconds
            && self.platform.modified_nanoseconds == current.platform.modified_nanoseconds;
        if !state_unchanged {
            return Ok(false);
        }

        // Unlinking an open Unix file updates ctime while leaving the handle's contents stable.
        Ok(
            (self.platform.changed_seconds == current.platform.changed_seconds
                && self.platform.changed_nanoseconds == current.platform.changed_nanoseconds)
                || current.platform.nlink == 0,
        )
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn matches_current_state(&self, file: &File) -> io::Result<bool> {
        Self::from_file(file).map(|current| current == *self)
    }

    #[cfg(unix)]
    pub(crate) fn from_file(file: &File) -> io::Result<Self> {
        use cap_std::fs::MetadataExt;

        let metadata = file.metadata()?;
        Ok(Self {
            regular: metadata.is_file(),
            platform: PlatformFingerprint {
                device: metadata.dev(),
                inode: metadata.ino(),
                nlink: metadata.nlink(),
                length: metadata.len(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                changed_seconds: metadata.ctime(),
                changed_nanoseconds: metadata.ctime_nsec(),
            },
        })
    }

    #[cfg(windows)]
    pub(crate) fn from_file(file: &File) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FILE_ID_INFO, FILE_STANDARD_INFO, FileBasicInfo, FileIdInfo,
            FileStandardInfo,
        };

        let handle = file.as_raw_handle();
        let id: FILE_ID_INFO = query_file_information(handle, FileIdInfo)?;
        let standard: FILE_STANDARD_INFO = query_file_information(handle, FileStandardInfo)?;
        let basic: FILE_BASIC_INFO = query_file_information(handle, FileBasicInfo)?;
        Ok(Self {
            regular: !standard.Directory,
            platform: PlatformFingerprint {
                volume: id.VolumeSerialNumber,
                file_id: id.FileId.Identifier,
                length: standard.EndOfFile,
                last_write_time: basic.LastWriteTime,
                change_time: basic.ChangeTime,
            },
        })
    }

    #[cfg(windows)]
    pub(crate) fn from_file_state(file: &File) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FILE_STANDARD_INFO, FileBasicInfo, FileStandardInfo,
        };

        let handle = file.as_raw_handle();
        let standard: FILE_STANDARD_INFO = query_file_information(handle, FileStandardInfo)?;
        let basic: FILE_BASIC_INFO = query_file_information(handle, FileBasicInfo)?;
        Ok(Self {
            regular: !standard.Directory,
            platform: PlatformFingerprint {
                volume: 0,
                file_id: [0; 16],
                length: standard.EndOfFile,
                last_write_time: basic.LastWriteTime,
                change_time: basic.ChangeTime,
            },
        })
    }

    #[cfg(not(windows))]
    pub(crate) fn from_file_state(file: &File) -> io::Result<Self> {
        Self::from_file(file)
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn from_file(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            regular: metadata.is_file(),
            platform: PlatformFingerprint {
                length: metadata.len(),
                modified: metadata.modified().ok().map(Into::into),
            },
        })
    }
}

#[cfg(windows)]
fn query_file_information<T: Default>(
    handle: windows_sys::Win32::Foundation::HANDLE,
    class: i32,
) -> io::Result<T> {
    use std::mem::size_of;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

    let mut value = T::default();
    let size = u32::try_from(size_of::<T>()).expect("file information size fits DWORD");
    #[cfg(feature = "bench-internals")]
    let started = std::time::Instant::now();
    // SAFETY: `handle` is borrowed from a live file, and `value` is writable for the structure
    // size corresponding to `class` at each call site.
    let succeeded =
        unsafe { GetFileInformationByHandleEx(handle, class, (&raw mut value).cast(), size) };
    #[cfg(feature = "bench-internals")]
    record_fingerprint_query(class, started.elapsed());
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}
