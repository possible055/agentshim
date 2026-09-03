//! Pure Rust reader for legacy Excel Binary (.xls) BIFF8 files.
//!
//! # Example
//!
//! ```no_run
//! use office_oxide::xls::XlsDocument;
//!
//! let doc = XlsDocument::open("spreadsheet.xls").unwrap();
//! println!("{}", doc.plain_text());
//! ```

mod cell;
mod error;
mod records;
mod sst;
mod workbook;

pub use error::XlsError;
pub use workbook::XlsDocument;
