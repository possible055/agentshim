use std::io::{self, Read, Seek, SeekFrom};

use cap_std::fs::File;
use grep_searcher::{BinaryDetection, Searcher};
use tokio_util::sync::CancellationToken;

use crate::output::SkipReason;

use super::{FileSearchContext, PlanSink, SearchError, SearchPlan};
use crate::tools::grep::profile::GrepStage;

/// How one candidate's bytes must be treated before the matcher sees them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SourceHandling {
    /// Bytes are searched as they are, through the existing fast paths.
    Native,
    /// Bytes are legacy-encoded text and must be decoded before searching.
    Transcode(&'static str),
    /// Nothing can read this file as text.
    Skip(SkipReason),
}

use crate::tools::grep::request::{GrepMemoryPolicy, GrepSourcePolicy};

const ENCODING_SNIFF_BYTES: usize = 8 * 1024;

/// Decide how one candidate's bytes must be treated before the matcher sees them.
///
/// A prefix that is already valid UTF-8 — which includes every pure-ASCII file — returns
/// `Native` and reaches the unchanged search paths below, so a UTF-8 tree pays only for
/// this one bounded read.
pub(super) fn classify_source(
    file: &mut File,
    plan: SearchPlan,
    cancellation: &CancellationToken,
) -> Result<SourceHandling, SearchError> {
    if let Some(label) = plan.encoding {
        return Ok(SourceHandling::Transcode(label));
    }
    if cancellation.is_cancelled() {
        return Err(SearchError::Cancelled);
    }
    let mut prefix = vec![0_u8; ENCODING_SNIFF_BYTES];
    let mut filled = 0_usize;
    while filled < prefix.len() {
        match file.read(&mut prefix[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(SearchError::Io),
        }
    }
    let whole_file = filled < prefix.len();
    prefix.truncate(filled);
    if file.seek(SeekFrom::Start(0)).is_err() {
        return Err(SearchError::Io);
    }
    // UTF-16 is transcoded by the searcher's own BOM sniffing, and anything else carrying
    // NUL is binary. Both must keep reaching the existing paths so they are still reported
    // as binary rather than as an encoding problem.
    if prefix.starts_with(&[0xFF, 0xFE]) || prefix.starts_with(&[0xFE, 0xFF]) {
        return Ok(SourceHandling::Native);
    }
    if prefix.contains(&0) {
        return Ok(SourceHandling::Native);
    }
    Ok(
        match crate::encoding::detect_legacy_encoding(&prefix, None, whole_file) {
            Ok(None) if !whole_file && plan.fallback_encoding.is_some() => {
                let valid_utf8 = crate::encoding::decode_stream(
                    &mut *file,
                    None,
                    usize::MAX,
                    cancellation,
                    |_| Ok(crate::encoding::DecodeControl::Continue),
                );
                if file.seek(SeekFrom::Start(0)).is_err() {
                    return Err(SearchError::Io);
                }
                match valid_utf8 {
                    Ok(_) => SourceHandling::Native,
                    Err(crate::encoding::DecodeError::Cancelled) => {
                        return Err(SearchError::Cancelled);
                    }
                    Err(crate::encoding::DecodeError::Io(_)) => return Err(SearchError::Io),
                    Err(_) => SourceHandling::Transcode(
                        plan.fallback_encoding.expect("checked fallback encoding"),
                    ),
                }
            }
            Ok(None) => SourceHandling::Native,
            Ok(Some(label)) => SourceHandling::Transcode(label),
            Err(_) => plan.fallback_encoding.map_or(
                SourceHandling::Skip(SkipReason::Undecodable),
                SourceHandling::Transcode,
            ),
        },
    )
}

/// Search a legacy-encoded candidate by decoding it to UTF-8 first.
///
/// Line structure survives decoding, so sink line numbers still address the source file.
/// Validation and search use fixed buffers and never retain the decoded whole file.
pub(super) fn search_transcoded(
    file: File,
    label: &'static str,
    context: &FileSearchContext<'_>,
    searcher: &mut Searcher,
    sink: &mut PlanSink<'_>,
) -> (Result<(), SearchError>, File) {
    let Some(encoding) = encoding_rs::Encoding::for_label_no_replacement(label.as_bytes()) else {
        return (Err(SearchError::Transcode(SkipReason::Undecodable)), file);
    };
    context.profiler.record_search_reader();
    context.profiler.record_legacy_stream();
    let source_span = context.profiler.span(GrepStage::SearchReaderWorker);
    searcher.set_binary_detection(BinaryDetection::none());
    let mut reader = crate::encoding::StrictTranscodingReader::new(
        file,
        encoding,
        context.cancellation,
        context.plan.memory.decode_input_bytes,
        context.plan.memory.decode_output_bytes,
    );
    let result = searcher.search_reader(context.matcher, &mut reader, sink);
    let validation = if result.is_ok() {
        io::copy(&mut reader, &mut io::sink()).map(|_| ())
    } else {
        Ok(())
    };
    let (file, failure) = reader.into_parts();
    searcher.set_binary_detection(BinaryDetection::quit(0));
    drop(source_span);
    let result = match failure {
        Some(crate::encoding::TranscodeFailure::Cancelled) => Err(SearchError::Cancelled),
        Some(crate::encoding::TranscodeFailure::Io) => Err(SearchError::Io),
        Some(
            crate::encoding::TranscodeFailure::Malformed
            | crate::encoding::TranscodeFailure::Binary,
        ) => Err(SearchError::Transcode(SkipReason::Undecodable)),
        None if validation.is_err() => Err(SearchError::Io),
        None => result,
    };
    (result, file)
}

pub(super) fn memory_source_limit(source: GrepSourcePolicy, memory: GrepMemoryPolicy) -> u64 {
    match source {
        GrepSourcePolicy::Hybrid => memory.memory_source_bytes as u64,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::CaptureLimit(bytes) => bytes,
        #[cfg(any(test, feature = "bench-internals"))]
        GrepSourcePolicy::Reader
        | GrepSourcePolicy::FileNever
        | GrepSourcePolicy::MmapAlways
        | GrepSourcePolicy::MmapThreshold(_) => 0,
    }
}
