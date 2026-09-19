use std::io;

use cap_std::fs::File;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileFingerprint {
    pub regular: bool,
    platform: PlatformFingerprint,
}

#[cfg(feature = "bench-internals")]
#[derive(Clone, Copy, Debug, Default)]
pub struct FingerprintMetrics {
    pub query_calls: usize,
    pub query_ns: u64,
}

#[cfg(all(feature = "bench-internals", windows))]
static FINGERPRINT_QUERY_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(all(feature = "bench-internals", windows))]
static FINGERPRINT_QUERY_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "bench-internals")]
pub fn reset_fingerprint_metrics() {
    #[cfg(windows)]
    {
        FINGERPRINT_QUERY_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
        FINGERPRINT_QUERY_NS.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(all(feature = "bench-internals", windows))]
#[must_use]
pub fn fingerprint_metrics() -> FingerprintMetrics {
    FingerprintMetrics {
        query_calls: FINGERPRINT_QUERY_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        query_ns: FINGERPRINT_QUERY_NS.load(std::sync::atomic::Ordering::Relaxed),
    }
}

#[cfg(all(feature = "bench-internals", not(windows)))]
#[must_use]
pub fn fingerprint_metrics() -> FingerprintMetrics {
    FingerprintMetrics::default()
}

#[cfg(all(feature = "bench-internals", windows))]
fn record_fingerprint_query(elapsed: std::time::Duration) {
    FINGERPRINT_QUERY_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let elapsed = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    let _ = FINGERPRINT_QUERY_NS.fetch_update(
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
    volume: u32,
    file_index: u64,
    length: u64,
    last_write_time: i64,
    // Renaming a file leaves its index, size, and write time alone while updating the
    // change time, so this is what lets a same-handle fingerprint notice a rename.
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
    pub fn source_id(&self) -> String {
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
            material.extend_from_slice(&self.platform.file_index.to_le_bytes());
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
    pub fn length(&self) -> u64 {
        self.platform.length
    }

    #[cfg(windows)]
    pub fn matches_current_state(&self, file: &File) -> io::Result<bool> {
        let current = Self::from_file(file)?;
        Ok(self.regular == current.regular
            && self.platform.length == current.platform.length
            && self.platform.last_write_time == current.platform.last_write_time
            && self.platform.change_time == current.platform.change_time)
    }

    #[cfg(unix)]
    pub fn matches_current_state(&self, file: &File) -> io::Result<bool> {
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

    /// True when the open file has been unlinked. Unix keeps such a handle's contents
    /// stable, so a read that captured its state before the unlink is still a coherent
    /// version even though the path is gone.
    #[cfg(unix)]
    pub(crate) fn unlinked(&self) -> bool {
        self.platform.nlink == 0
    }

    #[cfg(not(any(unix, windows)))]
    pub fn matches_current_state(&self, file: &File) -> io::Result<bool> {
        Self::from_file(file).map(|current| current == *self)
    }

    #[cfg(unix)]
    pub fn from_file(file: &File) -> io::Result<Self> {
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
    pub fn from_file(file: &File) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_BASIC_INFO, FileBasicInfo,
        };

        let handle = file.as_raw_handle();
        let info = query_by_handle(handle)?;
        let basic: FILE_BASIC_INFO = query_file_information(handle, FileBasicInfo)?;
        Ok(Self {
            regular: info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0,
            platform: PlatformFingerprint {
                volume: info.dwVolumeSerialNumber,
                file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
                length: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
                last_write_time: filetime_as_i64(info.ftLastWriteTime),
                change_time: basic.ChangeTime,
            },
        })
    }

    #[cfg(windows)]
    pub fn from_file_state(file: &File) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_BASIC_INFO, FileBasicInfo,
        };

        let handle = file.as_raw_handle();
        let info = query_by_handle(handle)?;
        let basic: FILE_BASIC_INFO = query_file_information(handle, FileBasicInfo)?;
        Ok(Self {
            regular: info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0,
            // State fingerprints carry no identity: grep compares them against the open
            // handle only, and the bench-only pathname-reopen policy reopens with
            // `from_file` when it needs identity.
            platform: PlatformFingerprint {
                volume: 0,
                file_index: 0,
                length: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
                last_write_time: filetime_as_i64(info.ftLastWriteTime),
                change_time: basic.ChangeTime,
            },
        })
    }

    #[cfg(not(windows))]
    pub fn from_file_state(file: &File) -> io::Result<Self> {
        Self::from_file(file)
    }

    #[cfg(not(any(unix, windows)))]
    pub fn from_file(file: &File) -> io::Result<Self> {
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
fn query_by_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

    let mut info = <windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION>::default();
    #[cfg(feature = "bench-internals")]
    let started = std::time::Instant::now();
    // SAFETY: `handle` is borrowed from a live file, and `info` is writable for the
    // structure `GetFileInformationByHandle` fills.
    let succeeded = unsafe { GetFileInformationByHandle(handle, &raw mut info) };
    #[cfg(feature = "bench-internals")]
    record_fingerprint_query(started.elapsed());
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(info)
    }
}

#[cfg(windows)]
fn query_file_information<T: Default>(
    handle: windows_sys::Win32::Foundation::HANDLE,
    class: i32,
) -> io::Result<T> {
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

    let mut value = T::default();
    let size = u32::try_from(std::mem::size_of::<T>()).expect("file information size fits DWORD");
    #[cfg(feature = "bench-internals")]
    let started = std::time::Instant::now();
    // SAFETY: `handle` is borrowed from a live file, and `value` is writable for the
    // structure size corresponding to `class` at each call site.
    let succeeded =
        unsafe { GetFileInformationByHandleEx(handle, class, (&raw mut value).cast(), size) };
    #[cfg(feature = "bench-internals")]
    record_fingerprint_query(started.elapsed());
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

#[cfg(windows)]
fn filetime_as_i64(time: windows_sys::Win32::Foundation::FILETIME) -> i64 {
    (i64::from(time.dwHighDateTime) << 32) | i64::from(time.dwLowDateTime)
}
