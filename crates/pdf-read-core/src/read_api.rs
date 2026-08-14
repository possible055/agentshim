use std::fs::File;

use crate::budget::PdfResourceLimits;
use crate::converters::ConversionOptions;
use crate::document::PdfDocument;
pub use crate::document::{PageTextAssessment, PageTextStatus, PageVisualAssessment};
use crate::error::Error;
pub use crate::error::LimitScope;
use crate::rendering::{render_page_fit, RenderOptions};

const RENDER_DPI: f32 = 150.0;

pub type Result<T> = std::result::Result<T, PdfReadError>;

/// Stable error categories exposed to the `read` tool integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PdfReadErrorKind {
    /// The input is not a structurally valid PDF.
    Invalid,
    /// The PDF uses a version, filter, or capability this core does not support.
    Unsupported,
    /// The PDF requires authentication.
    Encrypted,
    /// An input, cache, image, or output limit was exceeded.
    ResourceLimit,
    /// Text extraction, classification, or rendering failed.
    Processing,
    /// Reading the supplied file handle failed.
    Io,
    /// The caller cancelled, or the mode runtime ceiling elapsed.
    Cancelled,
}

/// Which budget refused an allocation, and by how much.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimitDetails {
    /// Budget that refused the allocation.
    pub resource: &'static str,
    /// How much of the call the refusal invalidates.
    pub scope: LimitScope,
    /// Ceiling for that budget.
    pub limit_bytes: u64,
    /// Amount the operation required.
    pub observed_bytes: u64,
}

/// Error returned by the read-only PDF interface.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PdfReadError {
    kind: PdfReadErrorKind,
    message: String,
    limit: Option<ResourceLimitDetails>,
}

impl PdfReadError {
    /// Return the stable category used for tool error mapping.
    #[must_use]
    pub fn kind(&self) -> PdfReadErrorKind {
        self.kind
    }

    /// Which budget refused the allocation, when the error is a resource limit.
    ///
    /// A caller that can only read the message cannot tell a payload cap from a stream
    /// cap, so the structured form travels with the error.
    #[must_use]
    pub fn limit(&self) -> Option<ResourceLimitDetails> {
        self.limit
    }

    fn resource_limit(message: impl Into<String>) -> Self {
        Self {
            kind: PdfReadErrorKind::ResourceLimit,
            message: message.into(),
            limit: None,
        }
    }
}

impl From<Error> for PdfReadError {
    fn from(error: Error) -> Self {
        if let Error::ResourceLimit {
            resource,
            scope,
            limit_bytes,
            observed_bytes,
        } = error
        {
            return Self {
                kind: PdfReadErrorKind::ResourceLimit,
                message: error.to_string(),
                limit: Some(ResourceLimitDetails {
                    resource,
                    scope,
                    limit_bytes,
                    observed_bytes,
                }),
            };
        }
        let kind = match error {
            Error::Cancelled => PdfReadErrorKind::Cancelled,
            Error::ResourceLimit { .. } => PdfReadErrorKind::ResourceLimit,
            Error::Io(_) => PdfReadErrorKind::Io,
            Error::EncryptedPdf => PdfReadErrorKind::Encrypted,
            Error::UnsupportedVersion(_) | Error::Unsupported(_) | Error::UnsupportedFilter(_) => {
                PdfReadErrorKind::Unsupported
            }
            Error::InvalidHeader(_)
            | Error::ParseError { .. }
            | Error::InvalidXref
            | Error::ObjectNotFound(_, _)
            | Error::InvalidObjectType { .. }
            | Error::UnexpectedEof
            | Error::InvalidPdf(_)
            | Error::CircularReference(_)
            | Error::RecursionLimitExceeded(_) => PdfReadErrorKind::Invalid,
            _ => PdfReadErrorKind::Processing,
        };
        Self {
            kind,
            message: error.to_string(),
            limit: None,
        }
    }
}

/// Bounds applied while opening and caching a PDF.
#[derive(Clone, Copy, Debug)]
pub struct ParserLimits {
    /// Maximum accepted file or in-memory input length.
    pub max_input_bytes: u64,
    /// Maximum estimated size of the parsed object cache.
    pub object_cache_bytes: usize,
    /// Maximum entries in each Form XObject cache.
    pub xobject_cache_entries: usize,
}

impl Default for ParserLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 32 * 1024 * 1024,
            // 16 MiB, not the former 64 MiB: the old default was the entire per-call
            // reservation for text mode, so the cache alone could consume the budget the
            // whole call is supposed to fit inside.
            object_cache_bytes: PdfResourceLimits::text().object_cache_bytes,
            xobject_cache_entries: 1024,
        }
    }
}

/// Visible page geometry in PDF points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageInfo {
    /// Visible width after applying the CropBox fallback rule.
    pub width_points: f32,
    /// Visible height after applying the CropBox fallback rule.
    pub height_points: f32,
    /// Page rotation in degrees.
    pub rotation_degrees: i32,
}

/// Options for model-oriented Markdown extraction.
#[derive(Clone, Copy, Debug)]
pub struct MarkdownOptions {
    /// Infer heading levels from font and layout signals.
    pub detect_headings: bool,
    /// Detect and format tables.
    pub extract_tables: bool,
    /// Include read-only form field values in their page positions.
    pub include_form_fields: bool,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            detect_headings: true,
            extract_tables: true,
            include_form_fields: true,
        }
    }
}

/// Hard pixel bounds for a rendered page.
#[derive(Clone, Copy, Debug)]
pub struct RenderLimits {
    /// Maximum output width.
    pub max_width_pixels: u32,
    /// Maximum output height.
    pub max_height_pixels: u32,
    /// Maximum output width multiplied by height.
    pub max_pixels: u64,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            max_width_pixels: 2048,
            max_height_pixels: 2048,
            max_pixels: 4_000_000,
        }
    }
}

/// A window of one page's Markdown.
#[derive(Clone, Debug)]
pub struct MarkdownChunk {
    /// Text for this window.
    pub text: String,
    /// Byte offset to resume at, or `None` when the page is complete.
    pub next_offset: Option<usize>,
    /// Total Markdown length for the page.
    pub page_bytes: usize,
}

/// PNG output produced by `render_page_fit`.
#[derive(Debug)]
pub struct RenderedPage {
    /// Encoded PNG bytes.
    pub png: Vec<u8>,
    /// Encoded image width.
    pub width_pixels: u32,
    /// Encoded image height.
    pub height_pixels: u32,
}

/// An immutable, file-backed PDF document for `codexshim` reads.
pub struct PdfReadDocument {
    document: PdfDocument,
}

impl PdfReadDocument {
    /// Open an existing file handle without buffering the whole PDF.
    pub fn from_file(file: File, limits: ParserLimits) -> Result<Self> {
        validate_parser_limits(limits)?;
        let input_bytes = file.metadata().map_err(Error::Io)?.len();
        if input_bytes > limits.max_input_bytes {
            return Err(PdfReadError::resource_limit(format!(
                "PDF input is {input_bytes} bytes; limit is {} bytes",
                limits.max_input_bytes
            )));
        }
        let document = PdfDocument::from_file_with_limits(
            file,
            limits.object_cache_bytes,
            limits.xobject_cache_entries,
        )?;
        Ok(Self { document })
    }

    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: Vec<u8>, limits: ParserLimits) -> Result<Self> {
        validate_parser_limits(limits)?;
        if bytes.len() as u64 > limits.max_input_bytes {
            return Err(PdfReadError::resource_limit(format!(
                "PDF input is {} bytes; limit is {} bytes",
                bytes.len(),
                limits.max_input_bytes
            )));
        }
        let document = PdfDocument::from_bytes(bytes)?;
        Ok(Self { document })
    }

    /// Return the number of pages.
    pub fn page_count(&self) -> Result<usize> {
        self.document.page_count().map_err(Into::into)
    }

    /// Drop per-page scratch held from earlier pages.
    ///
    /// The span, character, and content caches exist so repeated work on one page is not
    /// redone. A single forward pass never revisits a page, so once it is committed that
    /// scratch is only occupying the call budget.
    pub fn release_page_scratch(&self) {
        self.document.release_page_scratch();
        crate::budget::release_page_markdown();
    }

    /// Extract one zero-based page as Markdown.
    pub fn page_to_markdown(&self, page_index: usize, options: &MarkdownOptions) -> Result<String> {
        let mut conversion = ConversionOptions::default();
        conversion.preserve_layout = false;
        conversion.detect_headings = options.detect_headings;
        conversion.extract_tables = options.extract_tables;
        conversion.include_form_fields = options.include_form_fields;
        let markdown = self.document.to_markdown(page_index, &conversion)?;
        crate::budget::check_page_markdown(markdown.len())?;
        Ok(markdown)
    }

    /// Extract a bounded window of one page's Markdown, starting at a byte offset.
    ///
    /// Offsets index the page's own Markdown, so the envelope a caller wraps around it —
    /// path header, page heading, continuation metadata — never shifts them. The window
    /// is trimmed down to a UTF-8 boundary, so concatenating successive chunks
    /// reproduces `page_to_markdown` exactly.
    ///
    /// # Errors
    ///
    /// Returns a validation error when `offset` is past the end of the page or lands
    /// inside a multi-byte character.
    pub fn page_to_markdown_chunk(
        &self,
        page_index: usize,
        options: &MarkdownOptions,
        offset: usize,
        max_bytes: usize,
    ) -> Result<MarkdownChunk> {
        let markdown = self.page_to_markdown(page_index, options)?;
        if offset > markdown.len() {
            return Err(PdfReadError {
                kind: PdfReadErrorKind::Invalid,
                message: format!(
                    "resume offset {offset} is past the end of page {} ({} bytes)",
                    page_index + 1,
                    markdown.len()
                ),
                limit: None,
            });
        }
        if !markdown.is_char_boundary(offset) {
            return Err(PdfReadError {
                kind: PdfReadErrorKind::Invalid,
                message: format!(
                    "resume offset {offset} is not a character boundary on page {}",
                    page_index + 1
                ),
                limit: None,
            });
        }

        let remaining = &markdown[offset..];
        let mut end = max_bytes.min(remaining.len());
        while end > 0 && !remaining.is_char_boundary(end) {
            end -= 1;
        }
        let next_offset = (end < remaining.len()).then_some(offset + end);
        Ok(MarkdownChunk {
            text: remaining[..end].to_owned(),
            next_offset,
            page_bytes: markdown.len(),
        })
    }

    /// Judge one page's text usability, independent of anything visual.
    pub fn assess_page_text(&self, page_index: usize) -> Result<PageTextAssessment> {
        self.document
            .assess_page_text(page_index)
            .map_err(Into::into)
    }

    /// Judge what one page draws, from operators and resource dictionaries only.
    ///
    /// Never decompresses an image or rasterises, so it is safe on the text path.
    pub fn assess_page_visual(&self, page_index: usize) -> Result<PageVisualAssessment> {
        self.document
            .assess_page_visual(page_index)
            .map_err(Into::into)
    }

    /// Return visible geometry for one zero-based page.
    pub fn page_info(&self, page_index: usize) -> Result<PageInfo> {
        let info = self.document.get_page_info(page_index)?;
        let visible = info.crop_box.unwrap_or(info.media_box);
        Ok(PageInfo {
            width_points: visible.width,
            height_points: visible.height,
            rotation_degrees: info.rotation,
        })
    }

    /// Render one zero-based page as PNG within fixed pixel bounds.
    pub fn render_page_fit(&self, page_index: usize, limits: RenderLimits) -> Result<RenderedPage> {
        let (width, height) = self.render_dimensions(page_index, limits)?;
        // Estimated before the surface exists: RGBA output plus one scratch copy of the
        // same size, which is what the rasteriser peaks at. Checking after allocation
        // would already have spent what the check is meant to refuse.
        let surface_bytes = u64::from(width)
            .saturating_mul(u64::from(height))
            .saturating_mul(4)
            .saturating_mul(2);
        crate::budget::check_render_surface(usize::try_from(surface_bytes).unwrap_or(usize::MAX))?;
        crate::budget::check_cancelled()?;
        let options = RenderOptions::with_dpi(RENDER_DPI as u32);
        let rendered = render_page_fit(&self.document, page_index, width, height, &options)?;
        // The surface is gone once the page is encoded; holding its bytes against the
        // call total would make the second page of a render look like the first plus one.
        crate::budget::release_render_surface();
        crate::metrics::record_render(rendered.width, rendered.height, rendered.data.len());
        Ok(RenderedPage {
            png: rendered.data,
            width_pixels: rendered.width,
            height_pixels: rendered.height,
        })
    }

    fn render_dimensions(&self, page_index: usize, limits: RenderLimits) -> Result<(u32, u32)> {
        if limits.max_width_pixels == 0 || limits.max_height_pixels == 0 || limits.max_pixels == 0 {
            return Err(PdfReadError::resource_limit(
                "render width, height, and pixel limits must be positive",
            ));
        }
        let page = self.page_info(page_index)?;
        let rotation = page.rotation_degrees.rem_euclid(360);
        let (width_points, height_points) = if rotation == 90 || rotation == 270 {
            (page.height_points, page.width_points)
        } else {
            (page.width_points, page.height_points)
        };
        if !width_points.is_finite()
            || !height_points.is_finite()
            || width_points <= 0.0
            || height_points <= 0.0
        {
            return Err(PdfReadError {
                kind: PdfReadErrorKind::Invalid,
                message: "PDF page has invalid dimensions".to_string(),
                limit: None,
            });
        }

        let dpi_scale = RENDER_DPI / 72.0;
        let width_scale = limits.max_width_pixels as f32 / width_points;
        let height_scale = limits.max_height_pixels as f32 / height_points;
        let pixel_scale =
            (limits.max_pixels as f64 / (width_points as f64 * height_points as f64)).sqrt() as f32;
        let scale = dpi_scale
            .min(width_scale)
            .min(height_scale)
            .min(pixel_scale);
        let width = (width_points * scale).floor().max(1.0) as u32;
        let height = (height_points * scale).floor().max(1.0) as u32;
        Ok((width, height))
    }
}

fn validate_parser_limits(limits: ParserLimits) -> Result<()> {
    if limits.max_input_bytes == 0
        || limits.object_cache_bytes == 0
        || limits.xobject_cache_entries == 0
    {
        return Err(PdfReadError::resource_limit(
            "parser limits must all be positive",
        ));
    }
    Ok(())
}
