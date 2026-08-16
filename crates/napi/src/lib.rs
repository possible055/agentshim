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
mod spike;

pub use background::{ArtifactPublished, EngineJobHandle, NativeJobOutcome};
pub use classify::{RunnerFailureRule, SandboxAttribution};
pub use engine::{
    Engine, EngineOptions, EnvEntry, GlobArgs, GrepArgs, NativeImage, ReadArgs, ToolText,
};
pub use process::{ArtifactInfo, BashArgs, NativeFailure, ProcessArgs, ProcessOutcome};
pub use spike::{API_VERSION, api_version, spike_background_panic, spike_panic};
