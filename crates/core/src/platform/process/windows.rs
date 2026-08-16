#[path = "windows/detached.rs"]
mod detached;
#[path = "windows/platform.rs"]
mod platform;
#[path = "windows/runner.rs"]
mod runner;

#[cfg(test)]
pub use detached::set_after_primary_observation_hook_for_tests;
pub use detached::{DetachedTree, spawn_detached, spawn_detached_capture};
pub use runner::run;

#[cfg(test)]
#[path = "windows/tests.rs"]
mod test_suite;
