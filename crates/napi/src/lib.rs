//! Native engine for in-process `AgentShim` hosts.
//!
//! Phase-0 scope: prove the embedding seams — addon load, per-instance Engine
//! configuration with no process globals, a real core tool call, bounded
//! `ThreadsafeFunction` delivery, idempotent close, and panic containment at the
//! exported boundary. The full tool, capture, and job surface lands with the
//! later phases on top of the same Engine.

mod budget;
mod capture;
mod engine;
mod process;
mod spike;

pub use engine::{Engine, EngineOptions, EnvEntry, GlobArgs, GrepArgs, ReadArgs, ToolText};
pub use process::{ArtifactInfo, BashArgs, ProcessArgs, ProcessOutcome};
pub use spike::{API_VERSION, api_version, spike_background_panic, spike_panic};
