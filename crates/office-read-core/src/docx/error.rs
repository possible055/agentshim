use thiserror::Error;

/// Errors specific to DOCX processing.
#[derive(Debug, Error)]
pub enum DocxError {
    /// Error from the underlying OPC/XML layer.
    #[error(transparent)]
    Core(#[from] crate::core::Error),
}

/// Convenience alias for `Result<T, DocxError>`.
pub type Result<T> = std::result::Result<T, DocxError>;
