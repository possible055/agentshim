use thiserror::Error;

/// Errors specific to XLSX processing.
#[derive(Debug, Error)]
pub enum XlsxError {
    /// Error from the underlying OPC/XML layer.
    #[error(transparent)]
    Core(#[from] crate::core::Error),
}

/// Convenience alias for `Result<T, XlsxError>`.
pub type Result<T> = std::result::Result<T, XlsxError>;
