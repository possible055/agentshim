//! Native engine for in-process `AgentShim` hosts.
//!
//! Per-instance repository and process engine with explicit configuration,
//! durable capture artifacts, managed background jobs, and bounded teardown.

mod background;
mod budget;
mod capture;
mod classify;
mod engine;
mod process;

/// Module API version; hosts must exact-match before using any Engine capability.
pub const API_VERSION: u32 = 3;

#[napi_derive::napi]
pub fn api_version() -> u32 {
    API_VERSION
}

pub use background::{ArtifactPublished, EngineJobHandle, NativeJobOutcome};
pub use classify::{RunnerFailureRule, SandboxAttribution};
pub use engine::{
    Engine, EngineOptions, EnvEntry, GlobArgs, GrepArgs, NativeImage, ReadArgs, ToolText,
};
pub use process::{ArtifactInfo, BashArgs, NativeFailure, ProcessArgs, ProcessOutcome};
