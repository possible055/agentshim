use std::{fs::File, io, path::PathBuf};

#[cfg(windows)]
pub(crate) fn default_log_directory() -> io::Result<PathBuf> {
    default_log_directory_from(std::env::var_os("LOCALAPPDATA"))
}

#[cfg(windows)]
fn default_log_directory_from(local_app_data: Option<std::ffi::OsString>) -> io::Result<PathBuf> {
    local_app_data
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("agentshim").join("logs"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "LOCALAPPDATA must contain an absolute path",
            )
        })
}

#[cfg(unix)]
pub(crate) fn default_log_directory() -> io::Result<PathBuf> {
    default_log_directory_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

#[cfg(unix)]
fn default_log_directory_from(
    xdg_state_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> io::Result<PathBuf> {
    if let Some(path) = xdg_state_home.map(PathBuf::from) {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "XDG_STATE_HOME must be an absolute path",
            ));
        }
        return Ok(path.join("agentshim").join("logs"));
    }
    home.map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join(".local/state/agentshim/logs"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "HOME must contain an absolute path",
            )
        })
}

#[cfg_attr(
    windows,
    allow(
        clippy::unnecessary_wraps,
        reason = "the Unix implementation is fallible"
    )
)]
pub(crate) fn set_private_permissions(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
    }
    #[cfg(windows)]
    {
        let _ = file;
        Ok(())
    }
}

#[cfg(unix)]
pub(crate) fn try_lock_file(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    // Safety: the raw fd is borrowed from the live `file` for the duration of
    // the call, and `flock` only mutates the descriptor's lock state.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if matches!(error.kind(), io::ErrorKind::WouldBlock) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
pub(crate) fn unlock_file(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    // Safety: the raw fd is borrowed from the live `file` for the duration of
    // the call, and `flock` only mutates the descriptor's lock state.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn try_lock_file(file: &File) -> io::Result<bool> {
    use std::{mem::zeroed, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::{ERROR_LOCK_VIOLATION, GetLastError},
        Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx},
        System::IO::OVERLAPPED,
    };

    // Safety: `OVERLAPPED` is all-integer fields where an all-zero state is
    // the documented no-offset value for whole-file `LockFileEx` locking.
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    // Safety: the raw handle is borrowed from the live `file`, the overlapped
    // pointer stays valid for the synchronous call, and the lock range is the
    // documented whole-file form.
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &raw mut overlapped,
        )
    };
    if result != 0 {
        return Ok(true);
    }
    // Safety: a pure read of the calling thread's last-error slot.
    if unsafe { GetLastError() } == ERROR_LOCK_VIOLATION {
        Ok(false)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn unlock_file(file: &File) -> io::Result<()> {
    use std::{mem::zeroed, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{Storage::FileSystem::UnlockFileEx, System::IO::OVERLAPPED};

    // Safety: `OVERLAPPED` is all-integer fields where an all-zero state is
    // the documented no-offset value for whole-file `UnlockFileEx` ranges.
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    // Safety: the raw handle is borrowed from the live `file` and the
    // overlapped pointer stays valid for the synchronous call.
    let result = unsafe {
        UnlockFileEx(
            file.as_raw_handle(),
            0,
            u32::MAX,
            u32::MAX,
            &raw mut overlapped,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

    use super::*;

    #[test]
    fn default_log_directory_uses_the_platform_state_root() {
        let root = std::env::current_dir().expect("absolute test root");
        #[cfg(windows)]
        let directory = default_log_directory_from(Some(root.clone().into_os_string()))
            .expect("Windows state directory");
        #[cfg(unix)]
        let directory = default_log_directory_from(Some(root.clone().into_os_string()), None)
            .expect("Unix state directory");

        assert_eq!(directory, root.join("agentshim").join("logs"));
    }

    #[test]
    fn file_lock_is_exclusive_and_reusable() {
        let fixture = tempfile::tempdir().expect("fixture");
        let path = fixture.path().join("lock");
        let first = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .expect("first handle");
        let second = OpenOptions::new()
            .create(false)
            .read(true)
            .write(true)
            .open(&path)
            .expect("second handle");

        assert!(try_lock_file(&first).expect("first lock"));
        assert!(!try_lock_file(&second).expect("second lock"));
        unlock_file(&first).expect("first unlock");
        assert!(try_lock_file(&second).expect("reacquire"));
        unlock_file(&second).expect("second unlock");
    }

    #[cfg(unix)]
    #[test]
    fn private_permissions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let file = tempfile::tempfile().expect("temporary file");
        set_private_permissions(&file).expect("private permissions");
        assert_eq!(
            file.metadata().expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }
}
