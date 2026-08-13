pub(crate) enum Attempt<T> {
    Stable(T),
    Changed,
}

pub(super) enum PreparedKind {
    /// The mode is resolved here so admission knows which reservation and which runtime
    /// ceiling apply before any PDF work starts.
    Pdf {
        mode: PdfMode,
        /// Bytes this call reserves, and therefore the ceiling the core enforces.
        ///
        /// Resolved once, here, so the number charged against the shared pool and the
        /// number the parser is held to are the same number. Keeping them as two
        /// independent constants is how a configured reservation ends up describing
        /// nothing.
        call_bytes: usize,
    },
    Text {
        detected_encoding: Option<&'static str>,
    },
}

pub(crate) struct PreparedRead {
    resolved: ResolvedPath,
    absolute: String,
    file: File,
    before: FileFingerprint,
    prefix: Vec<u8>,
    kind: PreparedKind,
}

/// Per-mode reservations.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PdfMemoryBudgets {
    pub(crate) text_bytes: usize,
    pub(crate) image_bytes: usize,
}

impl PdfMemoryBudgets {
    pub(crate) fn from_config(config: &crate::runtime::RuntimeConfig) -> Self {
        Self {
            text_bytes: config.pdf_text_memory_bytes,
            image_bytes: config.pdf_image_memory_bytes,
        }
    }

    /// The shipped defaults, for paths that resolve a read without a running server.
    #[cfg(any(test, feature = "bench-internals"))]
    pub(crate) const fn defaults() -> Self {
        Self {
            text_bytes: crate::runtime::DEFAULT_PDF_TEXT_MEMORY_BYTES,
            image_bytes: crate::runtime::DEFAULT_PDF_IMAGE_MEMORY_BYTES,
        }
    }

    fn for_mode(self, mode: PdfMode) -> usize {
        match mode {
            PdfMode::Image => self.image_bytes,
            PdfMode::Auto | PdfMode::Text => self.text_bytes,
        }
    }
}

impl PreparedRead {
    pub(crate) fn pdf_mode(&self) -> Option<PdfMode> {
        match self.kind {
            PreparedKind::Pdf { mode, .. } => Some(mode),
            PreparedKind::Text { .. } => None,
        }
    }

    /// Bytes this call reserves from the shared pool.
    ///
    /// For a PDF this is the same number the core is held to, so the pool's view of the
    /// call and the parser's ceiling cannot drift apart.
    pub(crate) fn memory_charge(&self) -> usize {
        match self.kind {
            PreparedKind::Pdf { call_bytes, .. } => call_bytes,
            PreparedKind::Text { .. } => TEXT_READ_MEMORY_BYTES,
        }
    }

    pub(crate) fn runtime_limit(&self) -> Option<std::time::Duration> {
        let configured = match self.kind {
            PreparedKind::Pdf {
                mode: PdfMode::Image,
                ..
            } => crate::runtime::PDF_IMAGE_RUNTIME_LIMIT,
            PreparedKind::Pdf { .. } => crate::runtime::PDF_TEXT_RUNTIME_LIMIT,
            PreparedKind::Text { .. } => return None,
        };
        Some(forced_runtime_limit().unwrap_or(configured))
    }
}

pub(crate) fn prepare(
    access: &FileAccess,
    request: &ReadRequest,
    cancellation: &CancellationToken,
    budgets: PdfMemoryBudgets,
) -> Result<PreparedRead, ReadError> {
    request.validate()?;
    if cancellation.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    let resolved = access.resolve(Path::new(&request.path))?;
    let absolute = crate::path::display_path(resolved.absolute());
    let mut file = open_regular(access, &resolved)?;
    let before = FileFingerprint::from_file(&file)?;
    run_before_read_hook();

    let mut prefix = Vec::with_capacity(PREFIX_BYTES);
    file.by_ref()
        .take(PREFIX_BYTES as u64)
        .read_to_end(&mut prefix)?;
    let kind = if has_pdf_header(&prefix) || has_pdf_parameters(request) {
        let mode = request.pdf_mode.unwrap_or(PdfMode::Auto);
        PreparedKind::Pdf {
            mode,
            call_bytes: budgets.for_mode(mode),
        }
    } else {
        if has_binary_magic(&prefix) {
            return Err(ReadError::Binary);
        }
        let detected_encoding = detect_legacy_encoding(
            &prefix,
            request.encoding.as_deref(),
            before.length() <= prefix.len() as u64,
        )?;
        PreparedKind::Text { detected_encoding }
    };
    Ok(PreparedRead {
        resolved,
        absolute,
        file,
        before,
        prefix,
        kind,
    })
}

#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn execute_prepared(
    access: &FileAccess,
    request: &ReadRequest,
    prepared: PreparedRead,
    cancellation: &CancellationToken,
) -> Result<Attempt<ToolOutput>, ReadError> {
    execute_prepared_with_budget(
        access,
        request,
        prepared,
        cancellation,
        &crate::output::CallOutputBudget::standalone(),
    )
}

pub(crate) fn execute_prepared_with_budget(
    access: &FileAccess,
    request: &ReadRequest,
    mut prepared: PreparedRead,
    cancellation: &CancellationToken,
    output_budget: &crate::output::CallOutputBudget,
) -> Result<Attempt<ToolOutput>, ReadError> {
    if cancellation.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    if let PreparedKind::Pdf { call_bytes, .. } = prepared.kind {
        let output = read_pdf(
            &prepared.file,
            &prepared.absolute,
            request,
            &prepared.before.source_id(),
            cancellation,
            call_bytes,
            output_budget,
        )?;
        run_after_read_hook();
        if take_forced_change() {
            return Ok(Attempt::Changed);
        }
        return if source_is_unchanged(access, &prepared.resolved, &prepared.file, &prepared.before)?
        {
            Ok(Attempt::Stable(output))
        } else {
            Ok(Attempt::Changed)
        };
    }
    let PreparedKind::Text { detected_encoding } = prepared.kind else {
        unreachable!("PDF reads return before text decoding");
    };
    let reader = io::Cursor::new(prepared.prefix).chain(&mut prepared.file);
    let mut collector = LineCollector::new(request.start_line.unwrap_or(1), request.line_count);
    let summary = decode_stream(
        reader,
        request.encoding.as_deref().or(detected_encoding),
        usize::MAX,
        cancellation,
        |chunk| Ok(collector.push(chunk)),
    )?;
    collector.finish_eof();
    run_after_read_hook();

    if !source_is_unchanged(access, &prepared.resolved, &prepared.file, &prepared.before)? {
        return Ok(Attempt::Changed);
    }

    render(
        &prepared.absolute,
        request,
        summary.source_encoding,
        &collector,
        cancellation,
        output_budget,
    )
    .map(Attempt::Stable)
}

fn source_is_unchanged(
    access: &FileAccess,
    path: &ResolvedPath,
    file: &File,
    before: &FileFingerprint,
) -> Result<bool, ReadError> {
    if before != &FileFingerprint::from_file(file)? {
        return Ok(false);
    }
    match open_regular(access, path) {
        Ok(identity) => Ok(before == &FileFingerprint::from_file(&identity)?),
        Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn open_regular(access: &FileAccess, path: &ResolvedPath) -> Result<File, ReadError> {
    let metadata = access.symlink_metadata_kind(path)?;
    if metadata.is_dir {
        return Err(ReadError::Directory);
    }
    if path.is_ambient() && metadata.is_symlink {
        return Err(ReadError::NotRegular);
    }
    if !metadata.is_file && !metadata.is_symlink {
        return Err(ReadError::NotRegular);
    }
    let file = access.open_read(path)?;
    if !FileFingerprint::from_file(&file)?.regular {
        return Err(ReadError::NotRegular);
    }
    Ok(file)
}

pub(super) fn has_binary_magic(prefix: &[u8]) -> bool {
    prefix.starts_with(b"\x7FELF")
        || prefix.starts_with(b"MZ")
        || prefix.starts_with(b"\x89PNG\r\n\x1A\n")
        || prefix.starts_with(&[0xFF, 0xD8, 0xFF])
        || prefix.starts_with(b"GIF87a")
        || prefix.starts_with(b"GIF89a")
        || prefix.starts_with(b"BM")
        || prefix.starts_with(b"II*\0")
        || prefix.starts_with(b"MM\0*")
        || prefix.starts_with(b"RIFF") && prefix.get(8..12) == Some(b"WEBP")
}
use std::{
    io::{self, Read},
    path::Path,
};

use cap_std::fs::File;
use tokio_util::sync::CancellationToken;

use crate::{
    encoding::{decode_stream, detect_legacy_encoding},
    path::{FileAccess, ResolvedPath},
    tools::ToolOutput,
};

use super::{
    fingerprint::FileFingerprint,
    hooks::{forced_runtime_limit, run_after_read_hook, run_before_read_hook, take_forced_change},
    pdf::{has_pdf_header, has_pdf_parameters, read_pdf},
    request::{PREFIX_BYTES, PdfMode, ReadError, ReadRequest, TEXT_READ_MEMORY_BYTES},
    text::{LineCollector, render},
};
