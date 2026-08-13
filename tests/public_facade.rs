use std::path::PathBuf;

use codexshim::{
    CodexShim, CodexShimBuilder, DiagnosticsConfig, DiagnosticsGuard, LogMode, ReadScope,
    RuntimeLimits,
};

#[test]
fn intentional_public_facade_compiles() {
    let builder: fn(PathBuf) -> std::io::Result<CodexShimBuilder> = |root| CodexShim::builder(root);
    let _ = builder;
    let _: ReadScope = ReadScope::Normal;
    let _: fn() -> std::io::Result<RuntimeLimits> = RuntimeLimits::from_env;
    let _: fn() -> std::io::Result<DiagnosticsConfig> = DiagnosticsConfig::from_env;
    let _: fn(PathBuf) -> DiagnosticsGuard = DiagnosticsGuard::disabled;
    let _: LogMode = LogMode::Errors;
}
