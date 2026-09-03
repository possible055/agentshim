/// Errors when reading legacy DOC files.
#[derive(Debug, thiserror::Error)]
pub enum DocError {
    /// Error from the underlying CFB container reader.
    #[error("CFB error: {0}")]
    Cfb(#[from] crate::cfb::CfbError),

    /// The File Information Block (FIB) is malformed or unsupported.
    #[error("invalid FIB: {0}")]
    InvalidFib(String),

    /// The document uses an otherwise valid but unsupported Word version or feature.
    #[error("unsupported Word document: {0}")]
    Unsupported(String),

    /// The piece table (CLX/PCD) is malformed.
    #[error("invalid piece table: {0}")]
    InvalidPieceTable(String),

    /// A required CFB stream is absent from the file.
    #[error("missing stream: {0}")]
    MissingStream(String),

    /// Underlying I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience `Result` alias using `DocError`.
pub type Result<T> = std::result::Result<T, DocError>;
