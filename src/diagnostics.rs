mod core;
mod storage;

pub use core::{DiagnosticsConfig, DiagnosticsGuard, LogMode};
pub use storage::{capacity_bytes, purge, retention_days, status};
