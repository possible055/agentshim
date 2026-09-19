mod core;
mod storage;

pub(crate) use core::shell_delegate_class;
pub use core::{DiagnosticsConfig, DiagnosticsGuard, LogMode};
pub use storage::{capacity_bytes, purge, retention_days, status};
