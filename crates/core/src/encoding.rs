mod decoder;
mod io;

pub use decoder::{detect_legacy_encoding, normalize_label};
pub use io::{
    DecodeControl, DecodeError, SourceEncoding, StrictTranscodingReader, TranscodeFailure,
    decode_stream,
};

#[cfg(test)]
use io::decode_to_string;

#[cfg(test)]
mod tests;
