//! Native binding for the in-process DSH plugin.
//!
//! One plugin-owned host runtime shares capacity across per-cwd repository engines while
//! retaining explicit configuration, durable capture, managed jobs, and bounded teardown.

mod artifacts;
mod background;
mod budget;
mod capture;
mod classify;
mod config;
mod engine;
mod failures;
mod process;
mod tools;

/// Module API version; hosts must exact-match before using any Engine capability.
pub const API_VERSION: u32 = 5;

#[napi_derive::napi]
pub fn api_version() -> u32 {
    API_VERSION
}

pub use background::{ArtifactPublished, EngineJobHandle, NativeJobOutcome};
pub use classify::{RunnerFailureRule, SandboxAttribution};
pub use engine::{
    Engine, EnvEntry, GlobArgs, GrepArgs, NativeHostOptions, NativeHostRuntime, NativeImage,
    ReadArgs, ToolText,
};
pub use process::{ArtifactInfo, BashArgs, NativeFailure, ProcessArgs, ProcessOutcome};
