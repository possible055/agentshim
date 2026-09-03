use thiserror::Error;

/// Errors specific to PPTX processing.
#[derive(Debug, Error)]
pub enum PptxError {
    /// Error from the underlying OPC/XML layer.
    #[error(transparent)]
    Core(#[from] crate::core::Error),
}

/// Convenience alias for `Result<T, PptxError>`.
pub type Result<T> = std::result::Result<T, PptxError>;
