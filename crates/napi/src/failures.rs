//! Translation from core tool errors to the adapter's failure envelope.

use napi::{Error, Result};

pub(crate) fn read_failure(
    error: agentshim_core::tools::read::ReadError,
) -> crate::process::NativeFailure {
    use agentshim_core::tools::read::ReadError;

    match error {
        ReadError::Validation(message) => crate::process::NativeFailure::new(
            "INVALID_ARGS",
            message,
            false,
            Some(serde_json::json!({ "kind": "validation" })),
        ),
        ReadError::ResourceLimit {
            message,
            resource,
            limit_bytes,
            observed_bytes,
        } => crate::process::NativeFailure::new(
            "AGENTSHIM_READ_RESOURCE_LIMIT",
            message,
            false,
            Some(serde_json::json!({
                "kind": "resource",
                "resource": resource,
                "limitBytes": limit_bytes,
                "observedBytes": observed_bytes,
            })),
        ),
        ReadError::Cancelled => crate::process::NativeFailure::cancelled("read"),
        ReadError::ResourceBusy { resource, .. } => crate::process::NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            format!("read resource {resource} is busy"),
            true,
            Some(serde_json::json!({ "kind": "resource", "resource": resource })),
        ),
        ReadError::ResourceTimeout { limit, elapsed } => crate::process::NativeFailure::new(
            "AGENTSHIM_TIMEOUT",
            "PDF read exceeded its mode runtime limit",
            true,
            Some(serde_json::json!({
                "kind": "timeout",
                "operation": "read",
                "limitMs": u64::try_from(limit.as_millis()).unwrap_or(u64::MAX),
                "elapsedMs": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                "workStopped": true,
            })),
        ),
        ReadError::Worker(message) => crate::process::NativeFailure::new(
            "AGENTSHIM_NATIVE_THREAD_FAILED",
            message,
            true,
            Some(serde_json::json!({ "kind": "native_thread", "operation": "read" })),
        ),
        ReadError::Changed => crate::process::NativeFailure::new(
            "AGENTSHIM_FILE_CHANGED",
            "file changed during read",
            true,
            Some(serde_json::json!({ "kind": "consistency" })),
        ),
        error @ (ReadError::Directory | ReadError::NotRegular | ReadError::Binary) => {
            crate::process::NativeFailure::new(
                "AGENTSHIM_READ_TARGET_INVALID",
                error.to_string(),
                false,
                Some(serde_json::json!({ "kind": "target" })),
            )
        }
        ReadError::PdfImageRequired { pages, cursor } => crate::process::NativeFailure::new(
            "AGENTSHIM_PDF_IMAGE_REQUIRED",
            "selected PDF pages require image mode",
            false,
            Some(serde_json::json!({ "kind": "pdf", "pages": pages, "cursor": cursor })),
        ),
        ReadError::Path(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_READ_PATH_FAILED",
            error.to_string(),
            false,
            Some(serde_json::json!({ "kind": "path" })),
        ),
        ReadError::Io(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_READ_IO_FAILED",
            error.to_string(),
            true,
            Some(serde_json::json!({ "kind": "io", "ioKind": format!("{:?}", error.kind()) })),
        ),
        other => crate::process::NativeFailure::new(
            "AGENTSHIM_READ_FAILED",
            other.to_string(),
            true,
            Some(serde_json::json!({ "kind": "read" })),
        ),
    }
}

pub(crate) fn grep_failure(
    error: agentshim_core::tools::grep::GrepError,
) -> crate::process::NativeFailure {
    use agentshim_core::tools::grep::GrepError;

    match error {
        GrepError::Validation(message) => crate::process::NativeFailure::new(
            "INVALID_ARGS",
            message,
            false,
            Some(serde_json::json!({ "kind": "validation" })),
        ),
        GrepError::Regex(message) | GrepError::Glob(message) => crate::process::NativeFailure::new(
            "AGENTSHIM_GREP_PATTERN_INVALID",
            message,
            false,
            Some(serde_json::json!({ "kind": "pattern" })),
        ),
        error @ GrepError::CandidateMemory => crate::process::NativeFailure::new(
            "AGENTSHIM_GREP_RESOURCE_LIMIT",
            error.to_string(),
            false,
            Some(serde_json::json!({ "kind": "resource", "resource": "candidate_memory" })),
        ),
        error @ GrepError::MemoryBusy => crate::process::NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            error.to_string(),
            true,
            Some(serde_json::json!({ "kind": "resource", "resource": "grep_memory" })),
        ),
        GrepError::Cancelled => crate::process::NativeFailure::cancelled("grep"),
        GrepError::ResourceBusy(resource) => crate::process::NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            format!("grep resource {resource} is busy"),
            true,
            Some(serde_json::json!({ "kind": "resource", "resource": resource })),
        ),
        GrepError::Worker(message) => crate::process::NativeFailure::new(
            "AGENTSHIM_NATIVE_THREAD_FAILED",
            message,
            true,
            Some(serde_json::json!({ "kind": "native_thread", "operation": "grep" })),
        ),
        GrepError::Path(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_GREP_PATH_FAILED",
            error.to_string(),
            false,
            Some(serde_json::json!({ "kind": "path" })),
        ),
        GrepError::Io(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_GREP_IO_FAILED",
            error.to_string(),
            true,
            Some(serde_json::json!({ "kind": "io", "ioKind": format!("{:?}", error.kind()) })),
        ),
        other => crate::process::NativeFailure::new(
            "AGENTSHIM_GREP_FAILED",
            other.to_string(),
            true,
            Some(serde_json::json!({ "kind": "grep" })),
        ),
    }
}

pub(crate) fn glob_failure(
    error: agentshim_core::tools::glob::GlobError,
) -> crate::process::NativeFailure {
    use agentshim_core::tools::glob::GlobError;

    match error {
        GlobError::Validation(message) | GlobError::Pattern(message) => {
            crate::process::NativeFailure::new(
                "INVALID_ARGS",
                message,
                false,
                Some(serde_json::json!({ "kind": "pattern" })),
            )
        }
        error @ GlobError::Memory => crate::process::NativeFailure::new(
            "AGENTSHIM_GLOB_RESOURCE_LIMIT",
            error.to_string(),
            false,
            Some(serde_json::json!({ "kind": "resource", "resource": "glob_memory" })),
        ),
        error @ GlobError::MemoryBusy => crate::process::NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            error.to_string(),
            true,
            Some(serde_json::json!({ "kind": "resource", "resource": "glob_memory" })),
        ),
        GlobError::ResourceBusy(resource) => crate::process::NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            format!("glob resource {resource} is busy"),
            true,
            Some(serde_json::json!({ "kind": "resource", "resource": resource })),
        ),
        GlobError::Cancelled => crate::process::NativeFailure::cancelled("glob"),
        GlobError::Worker(message) => crate::process::NativeFailure::new(
            "AGENTSHIM_NATIVE_THREAD_FAILED",
            message,
            true,
            Some(serde_json::json!({ "kind": "native_thread", "operation": "glob" })),
        ),
        GlobError::Path(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_GLOB_PATH_FAILED",
            error.to_string(),
            false,
            Some(serde_json::json!({ "kind": "path" })),
        ),
        GlobError::Io(error) => crate::process::NativeFailure::new(
            "AGENTSHIM_GLOB_IO_FAILED",
            error.to_string(),
            true,
            Some(serde_json::json!({ "kind": "io", "ioKind": format!("{:?}", error.kind()) })),
        ),
        other => crate::process::NativeFailure::new(
            "AGENTSHIM_GLOB_FAILED",
            other.to_string(),
            true,
            Some(serde_json::json!({ "kind": "glob" })),
        ),
    }
}

pub(crate) fn parse_grep_mode(
    value: Option<&str>,
) -> Result<Option<agentshim_core::tools::grep::GrepMode>> {
    use agentshim_core::tools::grep::GrepMode;

    match value {
        None => Ok(None),
        Some("content") => Ok(Some(GrepMode::Content)),
        Some("files") => Ok(Some(GrepMode::Files)),
        Some("count") => Ok(Some(GrepMode::Count)),
        Some(other) => Err(Error::new(
            napi::Status::InvalidArg,
            format!("mode must be content, files, or count, got {other}"),
        )),
    }
}

pub(crate) fn parse_grep_case(
    value: Option<&str>,
) -> Result<Option<agentshim_core::tools::grep::CaseMode>> {
    use agentshim_core::tools::grep::CaseMode;

    match value {
        None => Ok(None),
        Some("smart") => Ok(Some(CaseMode::Smart)),
        Some("sensitive") => Ok(Some(CaseMode::Sensitive)),
        Some("insensitive") => Ok(Some(CaseMode::Insensitive)),
        Some(other) => Err(Error::new(
            napi::Status::InvalidArg,
            format!("case must be smart, sensitive, or insensitive, got {other}"),
        )),
    }
}

pub(crate) fn filter_capture_glob_lines(
    text: &str,
    repository_root: &std::path::Path,
    capture_root: &std::path::Path,
) -> String {
    text.lines()
        .filter(|line| {
            let candidate = std::path::Path::new(line);
            if line.starts_with("Partial:") || line.starts_with("Retry:") {
                return true;
            }
            let absolute = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                repository_root.join(candidate)
            };
            match std::fs::canonicalize(absolute) {
                Ok(path) => !path.starts_with(capture_root),
                Err(_) => true,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
