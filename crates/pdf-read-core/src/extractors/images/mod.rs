//! Image extraction from PDF XObject resources.
//!
//! This module provides functionality to extract images from PDF documents,
//! including JPEG pass-through for DCT-encoded images and raw pixel decoding
//! for other image types.
//!
//! Phase 5

use crate::error::{Error, Result};
use crate::extractors::ccitt_bilevel;
use crate::geometry::Rect;
use crate::object::ObjectRef;
use std::cmp::min;

mod codecs;
mod color_spaces;
mod decoding;
mod handles;
mod inline_images;
mod model;
mod parsing;

pub(crate) use codecs::decode_cmyk_jpeg_to_raw_cmyk;
pub use codecs::{
    cmyk_to_rgb, cmyk_to_rgb_with_transform, decode_cmyk_jpeg_to_rgb,
    decode_cmyk_jpeg_to_rgb_with_profile,
};
pub(crate) use color_spaces::{cmyk_pixel_to_rgb, lab_palette_to_rgb};
pub use decoding::extract_image_from_xobject;
pub(crate) use decoding::resolve_icc_profile_from_obj;
pub(crate) use handles::{image_handle_from_inline, image_handle_from_xobject, parse_filter_chain};
pub use handles::{PdfFilter, PdfImageHandle};
pub use inline_images::expand_inline_image_dict;
pub use model::{ColorSpace, ImageData, PdfImage, PixelFormat};
pub use parsing::parse_color_space;

use codecs::{decode_jbig2_image, decode_jpx_image};
use color_spaces::{
    expand_indexed_to_rgb_with_transform, extract_lab_whitepoint, reduce_16_to_8,
    resolve_indexed_palette, IndexedResolution,
};
use model::color_space_to_pixel_format;
use parsing::decode_array_inverts_1bpc;

#[cfg(test)]
use color_spaces::{expand_indexed_to_rgb, lab_pixel_to_rgb};
#[cfg(test)]
use handles::resolve_color_space_for_handle;

#[cfg(test)]
mod color_tests;
#[cfg(test)]
mod handle_tests;
