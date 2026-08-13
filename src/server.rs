mod catalog;
mod dispatch;
mod response;
mod service;

pub use service::{CodexShim, CodexShimBuilder};

#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct ToolsListCorrelation(pub String);
