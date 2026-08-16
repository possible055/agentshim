#[cfg(unix)]
#[path = "process/unix.rs"]
mod implementation;
#[cfg(windows)]
#[path = "process/windows.rs"]
mod implementation;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetachedObservation {
    pub tree_running: bool,
    pub primary_exit: Option<String>,
}

pub use implementation::*;
