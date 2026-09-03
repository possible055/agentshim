//! # office_oxide::core
//!
//! Shared primitives for OOXML document processing.
//!
//! Provides the Open Packaging Conventions (OPC) layer shared by
//! DOCX, XLSX, and PPTX formats: ZIP archive handling, content types,
//! relationships, core properties, and DrawingML shared types.

/// `[Content_Types].xml` parsing and writing.
pub mod content_types;
/// Shared `docProps/core.xml` generator used by DOCX, PPTX, XLSX writers.
/// Core error type and `Result` alias used throughout OOXML parsing.
pub mod error;
/// OPC (Open Packaging Conventions) reader and writer for ZIP-based packages.
pub mod opc;
/// Relationship parsing, lookup, and serialization (`.rels` files).
pub mod relationships;
/// Unit types: `Emu`, `Twip`, `HalfPoint`, `Percentage1000`, `Angle60k`.
pub mod units;
/// XML reading utilities, namespace constants, and attribute helpers.
pub mod xml;

pub use error::{Error, Result};
