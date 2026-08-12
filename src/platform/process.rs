#[cfg(unix)]
#[path = "process/unix.rs"]
mod implementation;
#[cfg(windows)]
#[path = "process/windows.rs"]
mod implementation;

pub(crate) use implementation::*;
