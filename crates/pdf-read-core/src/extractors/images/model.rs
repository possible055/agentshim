use super::*;

/// A PDF image with metadata and pixel data.
///
/// Represents an image extracted from a PDF, including dimensions,
/// color space information, and the actual image data (either JPEG
/// or raw pixels).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PdfImage {
    /// Image width in pixels
    width: u32,
    /// Image height in pixels
    height: u32,
    /// Color space of the image
    color_space: ColorSpace,
    /// Bits per color component (typically 8)
    bits_per_component: u8,
    /// Image data (JPEG or raw pixels)
    #[serde(skip_serializing_if = "ImageData::is_empty")]
    data: ImageData,
    /// Optional bounding box in PDF user space (v0.3.14)
    bbox: Option<Rect>,
    /// Rotation in degrees (v0.3.14)
    rotation_degrees: i32,
    /// Transformation matrix (v0.3.14)
    matrix: [f32; 6],
    /// CCITT decompression parameters (for 1-bit bilevel images)
    #[serde(skip)]
    ccitt_params: Option<crate::decoders::CcittParams>,
    /// Embedded ICC profile associated with the image's colour space,
    /// if any. For a plain `/ICCBased` image this is the profile from
    /// the array; for an `Indexed` image with an `ICCBased` base this
    /// is the base profile. `None` when the document only used
    /// device-dependent colour. Consumed by `save_as_*` to drive the
    /// CMYK→sRGB conversion through the CMM instead of the §10.3.5
    /// additive-clamp fallback.
    #[serde(skip)]
    icc_profile: Option<std::sync::Arc<crate::color::IccProfile>>,
    /// Rendering intent from the image dictionary's `/Intent`, or the
    /// graphics-state default per ISO 32000-1:2008 §8.6.5.8.
    rendering_intent: crate::color::RenderingIntent,
}

impl PdfImage {
    /// Create a new PDF image.
    pub fn new(
        width: u32,
        height: u32,
        color_space: ColorSpace,
        bits_per_component: u8,
        data: ImageData,
    ) -> Self {
        Self {
            width,
            height,
            color_space,
            bits_per_component,
            data,
            bbox: None,
            rotation_degrees: 0,
            matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            ccitt_params: None,
            icc_profile: None,
            rendering_intent: crate::color::RenderingIntent::default(),
        }
    }

    /// Create a new PDF image with spatial metadata (v0.3.14).
    pub fn with_spatial(
        width: u32,
        height: u32,
        color_space: ColorSpace,
        bits_per_component: u8,
        data: ImageData,
        bbox: Rect,
        rotation: i32,
        matrix: [f32; 6],
    ) -> Self {
        Self {
            width,
            height,
            color_space,
            bits_per_component,
            data,
            bbox: Some(bbox),
            rotation_degrees: rotation,
            matrix,
            ccitt_params: None,
            icc_profile: None,
            rendering_intent: crate::color::RenderingIntent::default(),
        }
    }

    /// Create a new PDF image with a bounding box (v0.3.12, convenience wrapper).
    pub fn with_bbox(
        width: u32,
        height: u32,
        color_space: ColorSpace,
        bits_per_component: u8,
        data: ImageData,
        bbox: Rect,
    ) -> Self {
        Self::with_spatial(
            width,
            height,
            color_space,
            bits_per_component,
            data,
            bbox,
            0,
            [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        )
    }

    /// Create a new PDF image with CCITT parameters.
    pub fn with_ccitt_params(
        width: u32,
        height: u32,
        color_space: ColorSpace,
        bits_per_component: u8,
        data: ImageData,
        ccitt_params: crate::decoders::CcittParams,
    ) -> Self {
        Self {
            width,
            height,
            color_space,
            bits_per_component,
            data,
            bbox: None,
            rotation_degrees: 0,
            matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            ccitt_params: Some(ccitt_params),
            icc_profile: None,
            rendering_intent: crate::color::RenderingIntent::default(),
        }
    }

    /// Get the image width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Get the image height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Get the image color space.
    pub fn color_space(&self) -> &ColorSpace {
        &self.color_space
    }

    /// Get bits per component.
    pub fn bits_per_component(&self) -> u8 {
        self.bits_per_component
    }

    /// Get the image data.
    pub fn data(&self) -> &ImageData {
        &self.data
    }

    /// Get the bounding box if available.
    pub fn bbox(&self) -> Option<&Rect> {
        self.bbox.as_ref()
    }

    /// Set the bounding box for this image.
    pub fn set_bbox(&mut self, bbox: Rect) {
        self.bbox = Some(bbox);
    }

    /// Get rotation in degrees.
    pub fn rotation_degrees(&self) -> i32 {
        self.rotation_degrees
    }

    /// Set rotation in degrees.
    pub fn set_rotation_degrees(&mut self, rotation: i32) {
        self.rotation_degrees = rotation;
    }

    /// Get transformation matrix.
    pub fn matrix(&self) -> [f32; 6] {
        self.matrix
    }

    /// Set transformation matrix.
    pub fn set_matrix(&mut self, matrix: [f32; 6]) {
        self.matrix = matrix;
    }

    /// Set CCITT decompression parameters for this image.
    pub fn set_ccitt_params(&mut self, params: crate::decoders::CcittParams) {
        self.ccitt_params = Some(params);
    }

    /// Get CCITT decompression parameters if available.
    pub fn ccitt_params(&self) -> Option<&crate::decoders::CcittParams> {
        self.ccitt_params.as_ref()
    }

    /// Embedded ICC profile associated with the image, if any.
    pub fn icc_profile(&self) -> Option<&std::sync::Arc<crate::color::IccProfile>> {
        self.icc_profile.as_ref()
    }

    /// Attach an ICC profile (used by extractors; colour conversion
    /// picks it up automatically when present).
    pub fn set_icc_profile(&mut self, profile: std::sync::Arc<crate::color::IccProfile>) {
        self.icc_profile = Some(profile);
    }

    /// Rendering intent — ISO 32000-1:2008 §8.6.5.8, defaults to
    /// `RelativeColorimetric`.
    pub fn rendering_intent(&self) -> crate::color::RenderingIntent {
        self.rendering_intent
    }

    /// Set the rendering intent (used by extractors when they see an
    /// explicit `/Intent` entry on the image dictionary).
    pub fn set_rendering_intent(&mut self, intent: crate::color::RenderingIntent) {
        self.rendering_intent = intent;
    }

    /// Build the source→sRGB transform from this image's embedded ICC
    /// profile (if any). Returns `None` when the image uses purely
    /// device-dependent colour, or when no profile was resolved at
    /// extraction time.
    ///
    /// The resulting transform is component-agnostic: callers pick the
    /// matching `Transform::convert_{cmyk,rgb,gray}_*` method based on
    /// the source pixel format. Used by the `decode_cmyk_jpeg_to_rgb_…`,
    /// `cmyk_to_rgb_with_transform`, and `save_raw_as_*` paths.
    fn build_icc_transform(&self) -> Option<crate::color::Transform> {
        self.icc_profile
            .as_ref()
            .map(|p| crate::color::Transform::new_srgb_target(p.clone(), self.rendering_intent))
    }

    /// Convert image to PNG bytes in memory.
    pub fn to_png_bytes(&self) -> Result<Vec<u8>> {
        use image::codecs::png::{CompressionType, FilterType, PngEncoder};
        use image::ImageEncoder;
        use std::io::Cursor;

        let mut buffer = Cursor::new(Vec::new());
        let encoder =
            PngEncoder::new_with_quality(&mut buffer, CompressionType::Fast, FilterType::NoFilter);

        match &self.data {
            ImageData::Raw { pixels, format } => {
                let expected_gray = (self.width * self.height) as usize;
                let expected_rgb = expected_gray * 3;

                if *format == PixelFormat::Grayscale
                    && matches!(
                        self.color_space,
                        ColorSpace::DeviceGray | ColorSpace::CalGray
                    )
                    && pixels.len() == expected_gray
                {
                    // image 0.25 changed `write_image` to take
                    // `ExtendedColorType` — `ColorType::*` now converts
                    // through `Into`. API-only change, same semantics.
                    encoder
                        .write_image(pixels, self.width, self.height, image::ColorType::L8.into())
                        .map_err(|e| Error::Encode(format!("Failed to encode PNG: {}", e)))?;
                } else if *format == PixelFormat::RGB && pixels.len() == expected_rgb {
                    encoder
                        .write_image(
                            pixels,
                            self.width,
                            self.height,
                            image::ColorType::Rgb8.into(),
                        )
                        .map_err(|e| Error::Encode(format!("Failed to encode PNG: {}", e)))?;
                } else {
                    let dynamic_image = self.to_dynamic_image()?;
                    let rgb = dynamic_image.to_rgb8();
                    // `ImageBuffer::from_raw` accepts a buffer at least as long
                    // as the image needs, so a mis-declared depth can leave the
                    // RGB buffer larger than width×height×3. Reject that here
                    // with a recoverable error instead of handing the encoder a
                    // mismatched buffer, which it asserts on (panicking through
                    // the FFI boundary where callers cannot catch it).
                    if rgb.as_raw().len() != expected_rgb {
                        return Err(Error::Encode(format!(
                            "image buffer length {} does not match {}x{} RGB image ({} expected)",
                            rgb.as_raw().len(),
                            self.width,
                            self.height,
                            expected_rgb
                        )));
                    }
                    encoder
                        .write_image(
                            rgb.as_raw(),
                            self.width,
                            self.height,
                            image::ColorType::Rgb8.into(),
                        )
                        .map_err(|e| Error::Encode(format!("Failed to encode PNG: {}", e)))?;
                }
            }
            ImageData::Jpeg(_) => {
                let dynamic_image = self.to_dynamic_image()?;
                let rgb = dynamic_image.to_rgb8();
                encoder
                    .write_image(
                        rgb.as_raw(),
                        self.width,
                        self.height,
                        image::ColorType::Rgb8.into(),
                    )
                    .map_err(|e| Error::Encode(format!("Failed to encode PNG: {}", e)))?;
            }
        }

        Ok(buffer.into_inner())
    }

    /// Convert this PDF image to a `DynamicImage`.
    pub fn to_dynamic_image(&self) -> Result<image::DynamicImage> {
        match &self.data {
            ImageData::Jpeg(jpeg_data) => {
                if self.color_space.components() == 4 {
                    // 4-component (DeviceCMYK / ICCBased N=4) JPEGs must go
                    // through the Adobe-aware CMYK path. image::load_from_memory
                    // routes them through zune-jpeg's own CMYK->RGB, which does
                    // not honor the APP14 inversion and yields near-black output
                    // (see decode_cmyk_jpeg_to_rgb_with_profile).
                    let transform = self.build_icc_transform();
                    let rgb = decode_cmyk_jpeg_to_rgb_with_profile(jpeg_data, transform.as_ref())?;
                    return image::ImageBuffer::<image::Rgb<u8>, Vec<u8>>::from_raw(
                        self.width,
                        self.height,
                        rgb,
                    )
                    .ok_or_else(|| Error::Decode("Invalid CMYK image dimensions".to_string()))
                    .map(image::DynamicImage::ImageRgb8);
                }
                log::debug!(
                    "Decoding JPEG data ({} bytes), starts with: {:02X?}",
                    jpeg_data.len(),
                    &jpeg_data[..min(jpeg_data.len(), 16)]
                );
                image::load_from_memory(jpeg_data)
                    .map_err(|e| Error::Decode(format!("Failed to decode JPEG: {}", e)))
            }
            ImageData::Raw { pixels, format } => {
                if self.bits_per_component == 1
                    && matches!(self.color_space, ColorSpace::DeviceGray)
                    && self.ccitt_params.is_some()
                {
                    // Only genuinely CCITT-filtered images reach here — see
                    // `extract_image_from_xobject`, which sets `ccitt_params`
                    // solely when the XObject's `/Filter` is CCITTFaxDecode.
                    let Some(params) = self.ccitt_params.clone() else {
                        unreachable!("guarded by ccitt_params.is_some() above")
                    };

                    let decompressed = ccitt_bilevel::decompress_ccitt(pixels, &params)?;
                    let grayscale =
                        ccitt_bilevel::bilevel_to_grayscale(&decompressed, self.width, self.height);

                    image::ImageBuffer::<image::Luma<u8>, Vec<u8>>::from_raw(
                        self.width,
                        self.height,
                        grayscale,
                    )
                    .ok_or_else(|| Error::Decode("Invalid image dimensions".to_string()))
                    .map(image::DynamicImage::ImageLuma8)
                } else if self.bits_per_component == 1
                    && matches!(self.color_space, ColorSpace::DeviceGray)
                {
                    // Non-CCITT 1-bit DeviceGray: the stream is already fully
                    // decoded (Flate/LZW/ASCII/no filter) to raw packed bits,
                    // one row per `ceil(width / 8)` bytes, MSB first. /Decode
                    // inversion (if any) was already folded into the bits by
                    // `extract_image_from_xobject`, so unpack with fixed
                    // ISO 32000-1 §8.9.5.2 Table 90 default semantics: sample
                    // bit 0 -> component 0.0 (black), bit 1 -> component 1.0
                    // (white).
                    let row_bytes = (self.width as usize).div_ceil(8);
                    let mut grayscale =
                        Vec::with_capacity(self.width as usize * self.height as usize);
                    for row in 0..self.height as usize {
                        let row_start = row * row_bytes;
                        for col in 0..self.width as usize {
                            let byte_idx = row_start + col / 8;
                            let bit = pixels
                                .get(byte_idx)
                                .map(|b| (b >> (7 - (col % 8))) & 1)
                                .unwrap_or(1);
                            grayscale.push(if bit == 0 { 0x00 } else { 0xFF });
                        }
                    }

                    image::ImageBuffer::<image::Luma<u8>, Vec<u8>>::from_raw(
                        self.width,
                        self.height,
                        grayscale,
                    )
                    .ok_or_else(|| Error::Decode("Invalid image dimensions".to_string()))
                    .map(image::DynamicImage::ImageLuma8)
                } else {
                    match (format, self.color_space) {
                        (PixelFormat::RGB, ColorSpace::DeviceRGB) => {
                            image::ImageBuffer::<image::Rgb<u8>, Vec<u8>>::from_raw(
                                self.width,
                                self.height,
                                pixels.clone(),
                            )
                            .ok_or_else(|| Error::Decode("Invalid image dimensions".to_string()))
                            .map(image::DynamicImage::ImageRgb8)
                        }
                        (PixelFormat::Grayscale, ColorSpace::DeviceGray) => {
                            image::ImageBuffer::<image::Luma<u8>, Vec<u8>>::from_raw(
                                self.width,
                                self.height,
                                pixels.clone(),
                            )
                            .ok_or_else(|| Error::Decode("Invalid image dimensions".to_string()))
                            .map(image::DynamicImage::ImageLuma8)
                        }
                        _ => {
                            let rgb_pixels = match format {
                                PixelFormat::Grayscale => {
                                    pixels.iter().flat_map(|&g| vec![g, g, g]).collect()
                                }
                                PixelFormat::CMYK => cmyk_to_rgb_with_transform(
                                    pixels,
                                    self.build_icc_transform().as_ref(),
                                ),
                                PixelFormat::RGB => pixels.clone(),
                            };
                            image::ImageBuffer::<image::Rgb<u8>, Vec<u8>>::from_raw(
                                self.width,
                                self.height,
                                rgb_pixels,
                            )
                            .ok_or_else(|| Error::Decode("Invalid image dimensions".to_string()))
                            .map(image::DynamicImage::ImageRgb8)
                        }
                    }
                }
            }
        }
    }
}

/// Image data representation.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum ImageData {
    /// JPEG-encoded image data.
    Jpeg(Vec<u8>),
    /// Raw pixel data with a specified format.
    Raw {
        /// Raw pixel bytes.
        pixels: Vec<u8>,
        /// Pixel format (RGB, Grayscale, CMYK).
        format: PixelFormat,
    },
}

impl ImageData {
    /// Returns true if the image data is empty.
    pub fn is_empty(&self) -> bool {
        match self {
            ImageData::Jpeg(data) => data.is_empty(),
            ImageData::Raw { pixels, .. } => pixels.is_empty(),
        }
    }
}

/// PDF color space types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ColorSpace {
    /// RGB color space (3 components).
    DeviceRGB,
    /// Grayscale color space (1 component).
    DeviceGray,
    /// CMYK color space (4 components).
    DeviceCMYK,
    /// Indexed (palette-based) color space.
    Indexed,
    /// Calibrated grayscale.
    CalGray,
    /// Calibrated RGB.
    CalRGB,
    /// CIE L*a*b* color space.
    Lab,
    /// ICC profile-based color space with N components.
    ICCBased(usize),
    /// Separation (spot color) space.
    Separation,
    /// DeviceN (multi-ink) color space.
    DeviceN,
    /// Pattern color space.
    Pattern,
}

impl ColorSpace {
    /// Returns the number of color components for this color space.
    pub fn components(&self) -> usize {
        match self {
            ColorSpace::DeviceGray => 1,
            ColorSpace::DeviceRGB => 3,
            ColorSpace::DeviceCMYK => 4,
            ColorSpace::Indexed => 1,
            ColorSpace::CalGray => 1,
            ColorSpace::CalRGB => 3,
            ColorSpace::Lab => 3,
            ColorSpace::ICCBased(n) => *n,
            ColorSpace::Separation => 1,
            ColorSpace::DeviceN => 4,
            ColorSpace::Pattern => 0,
        }
    }
}

/// Pixel format for raw image data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[allow(clippy::upper_case_acronyms)]
pub enum PixelFormat {
    /// RGB format (3 bytes per pixel).
    RGB,
    /// Grayscale format (1 byte per pixel).
    Grayscale,
    /// CMYK format (4 bytes per pixel).
    CMYK,
}

impl PixelFormat {
    /// Returns the number of bytes per pixel for this format.
    pub fn bytes_per_pixel(&self) -> usize {
        match self {
            PixelFormat::Grayscale => 1,
            PixelFormat::RGB => 3,
            PixelFormat::CMYK => 4,
        }
    }
}

pub(super) fn color_space_to_pixel_format(color_space: &ColorSpace) -> PixelFormat {
    match color_space {
        ColorSpace::DeviceGray => PixelFormat::Grayscale,
        ColorSpace::DeviceRGB => PixelFormat::RGB,
        ColorSpace::DeviceCMYK => PixelFormat::CMYK,
        ColorSpace::Indexed => PixelFormat::RGB,
        ColorSpace::CalGray => PixelFormat::Grayscale,
        ColorSpace::CalRGB => PixelFormat::RGB,
        ColorSpace::Lab => PixelFormat::RGB,
        ColorSpace::ICCBased(n) => match n {
            1 => PixelFormat::Grayscale,
            3 => PixelFormat::RGB,
            4 => PixelFormat::CMYK,
            _ => PixelFormat::RGB,
        },
        ColorSpace::Separation => PixelFormat::Grayscale,
        ColorSpace::DeviceN => PixelFormat::CMYK,
        ColorSpace::Pattern => PixelFormat::RGB,
    }
}
