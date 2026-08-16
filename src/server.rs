mod catalog;
mod dispatch;
mod response;
mod service;

pub use service::{AgentShim, AgentShimBuilder};

#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct ToolsListCorrelation(pub String);
