mod detached;
mod platform;
mod runner;

pub(crate) use detached::{DetachedTree, spawn_detached};
pub(crate) use runner::run;

#[cfg(test)]
#[path = "windows/tests.rs"]
mod test_suite;
