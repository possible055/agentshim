#[cfg(unix)]
#[path = "process/unix.rs"]
mod implementation;
#[cfg(windows)]
#[path = "process/windows.rs"]
mod implementation;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DetachedObservation {
    pub(crate) tree_running: bool,
    pub(crate) primary_exit: Option<String>,
}

pub(crate) use implementation::*;
