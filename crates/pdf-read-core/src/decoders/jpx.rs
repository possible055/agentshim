//! JPEG 2000 (`/JPXDecode`) image decoding via hayro-jpeg2000.
//!
//! ISO 32000-1 §7.4.9: a `/JPXDecode` stream is a JPEG 2000 codestream — either a
//! raw J2K codestream or a JP2-boxed file. hayro-jpeg2000 handles both. This decodes
//! the codestream to interleaved 8-bit-per-component samples; the caller maps the
//! component count to a colour space and applies `/Decode`, `/SMask`, etc.
//!
//! Feature-gated (`jpeg2000`): when the feature is off the call site returns the
//! existing `UnsupportedFilter` error rather than panicking.

#[cfg(feature = "jpeg2000")]
use crate::error::Error;
use crate::error::Result;

/// Pass-through filter for `/JPXDecode`.
///
/// Like `DCTDecode`/`JBIG2Decode`, the JPEG 2000 codestream is not decompressed
/// by the generic filter pipeline — it is handed to the image extractor, which
/// decodes it with hayro-jpeg2000 (`decode_jpx`). So this decoder returns its input
/// unchanged. It is always available (even without the `jpeg2000` feature) so the
/// pipeline can surface the codestream; the extractor's feature-gated path then
/// either decodes it or returns a typed `UnsupportedFilter` error.
pub struct JpxDecoder;

impl super::StreamDecoder for JpxDecoder {
    fn decode(&self, input: &[u8]) -> Result<Vec<u8>> {
        Ok(input.to_vec())
    }

    fn name(&self) -> &str {
        "JPXDecode"
    }
}

/// A decoded JPEG 2000 image: interleaved 8-bit samples plus component count.
#[cfg(feature = "jpeg2000")]
pub struct JpxImage {
    /// `width * height * num_components` bytes, component-interleaved (row-major).
    pub samples: Vec<u8>,
    pub num_components: u8,
}

/// Decode a JP2/J2K codestream to interleaved 8-bit-per-component samples.
#[cfg(feature = "jpeg2000")]
pub fn decode_jpx(bytes: &[u8]) -> Result<JpxImage> {
    use hayro_jpeg2000::{DecodeSettings, Image};

    let image = Image::new(bytes, &DecodeSettings::default()).map_err(|e| {
        Error::UnsupportedFilter(format!("JPXDecode: JPEG 2000 decode failed: {e:?}"))
    })?;

    let pixel_count = image.width() as usize * image.height() as usize;
    if pixel_count == 0 {
        return Err(Error::UnsupportedFilter(
            "JPXDecode: JPEG 2000 image has no pixels".to_string(),
        ));
    }
    let samples = image.decode().map_err(|e| {
        Error::UnsupportedFilter(format!("JPXDecode: JPEG 2000 decode failed: {e:?}"))
    })?;
    if samples.is_empty() || !samples.len().is_multiple_of(pixel_count) {
        return Err(Error::UnsupportedFilter(
            "JPXDecode: decoded sample count does not match image dimensions".to_string(),
        ));
    }
    let num_components = samples.len() / pixel_count;
    let num_components = u8::try_from(num_components).map_err(|_| {
        Error::UnsupportedFilter("JPXDecode: too many image components".to_string())
    })?;
    Ok(JpxImage {
        samples,
        num_components,
    })
}
