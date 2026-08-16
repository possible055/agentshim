mod capture;
mod catalog;
mod dispatch;
mod response;
mod service;
pub(crate) use capture::{DshCaptureRequest, RemoteCaptureSink};
pub(crate) use service::DSH_BRIDGE_VERSION;

pub use service::{AgentShim, AgentShimBuilder};

#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct ToolsListCorrelation(pub String);
