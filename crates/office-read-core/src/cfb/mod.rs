//! Pure Rust reader for Compound Binary File Format (CFBF/OLE2) containers.
//!
//! This crate provides read access to the OLE2 structured storage format used by
//! legacy Microsoft Office files (.doc, .xls, .ppt) and other applications.
//!
//! # Example
//!
//! ```no_run
//! use std::fs::File;
//! use office_oxide::cfb::CfbReader;
//!
//! let file = File::open("spreadsheet.xls").unwrap();
//! let mut reader = CfbReader::new(file).unwrap();
//! let workbook = reader.open_stream("Workbook").unwrap();
//! ```

mod directory;
mod error;
mod header;
mod reader;

pub use error::CfbError;
pub use header::CFB_SIGNATURE;
pub use reader::CfbReader;
