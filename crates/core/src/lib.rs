//! Host-neutral repository tool engine shared by the `agentshim` MCP shell and
//! native hosts. The crate carries no MCP, client-profile, tokenizer, or transport
//! concepts; every output budget and timeout ceiling arrives as an explicit
//! per-instance parameter.

pub mod dsh_bridge;
pub mod encoding;
pub mod output;
pub mod path;
pub mod platform;
pub mod runtime;
pub mod sorting;
pub mod tools;
pub mod traversal;
