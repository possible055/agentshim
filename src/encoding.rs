mod decoder;
mod io;

pub(crate) use decoder::{detect_legacy_encoding, normalize_label};
pub(crate) use io::{DecodeControl, DecodeError, SourceEncoding, decode_stream};

#[cfg(test)]
use io::decode_to_string;

#[cfg(test)]
mod tests;
