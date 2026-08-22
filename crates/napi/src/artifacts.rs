//! Capture artifact serving: capability lookup, capture-path containment,
//! and base64 paging over published artifacts.

use std::{
    io::{Read as _, Seek as _, SeekFrom},
    sync::Arc,
};

use base64::Engine as _;

use agentshim_core::output::CallBudget as _;

use crate::{capture::ArtifactRecord, engine::EngineState, process::NativeFailure};

impl EngineState {
    /// Resolve the file-access view a text tool runs under, granting an exact
    /// capability when the target is a published capture artifact.
    pub(crate) fn granted_access(
        &self,
        record: Option<&ArtifactRecord>,
        error_code: &'static str,
    ) -> std::result::Result<Arc<agentshim_core::path::FileAccess>, NativeFailure> {
        match record {
            Some(record) => Ok(Arc::new(
                self.access
                    .with_exact_grant(&record.path)
                    .map_err(|error| {
                        NativeFailure::new(
                            error_code,
                            error.to_string(),
                            false,
                            Some(serde_json::json!({ "kind": "path" })),
                        )
                    })?,
            )),
            None => Ok(Arc::clone(&self.access)),
        }
    }

    pub(crate) fn engine_with_access(
        &self,
        access: Arc<agentshim_core::path::FileAccess>,
        error_code: &'static str,
    ) -> std::result::Result<agentshim_core::ToolEngine, NativeFailure> {
        self.tool_engine.with_file_access(access).map_err(|error| {
            NativeFailure::new(
                error_code,
                error.to_string(),
                false,
                Some(serde_json::json!({ "kind": "path" })),
            )
        })
    }

    pub(crate) fn artifact(&self, requested: &str) -> Option<ArtifactRecord> {
        let requested = std::fs::canonicalize(requested).ok()?;
        self.artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|record| {
                std::fs::canonicalize(&record.path).is_ok_and(|published| published == requested)
            })
            .cloned()
    }

    pub(crate) fn is_capture_path(&self, requested: &str) -> bool {
        let requested = std::path::Path::new(requested);
        let absolute = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.path().join(requested)
        };
        std::fs::canonicalize(absolute).is_ok_and(|path| path.starts_with(&self.capture_root))
    }

    pub(crate) fn read_artifact_page(
        &self,
        record: &ArtifactRecord,
        offset: Option<f64>,
    ) -> std::result::Result<crate::engine::ToolText, NativeFailure> {
        let offset = offset.unwrap_or(0.0);
        if !offset.is_finite() || offset < 0.0 || offset.fract() != 0.0 {
            return Err(NativeFailure::invalid(
                "artifactOffset must be a non-negative integer",
            ));
        }
        let offset = offset as u64;
        if offset > record.bytes {
            return Err(NativeFailure::invalid(
                "artifactOffset is beyond the artifact snapshot",
            ));
        }
        let metadata = std::fs::symlink_metadata(&record.path).map_err(|error| {
            NativeFailure::new(
                "AGENTSHIM_READ_IO_FAILED",
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "io", "operation": "artifact_metadata" })),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(NativeFailure::new(
                "AGENTSHIM_READ_TARGET_INVALID",
                "published artifact is no longer a regular file",
                false,
                Some(serde_json::json!({ "kind": "artifact" })),
            ));
        }
        let wrapper_bytes = 512_usize;
        let encoded_budget = self
            .output_limits
            .page_bytes()
            .saturating_sub(wrapper_bytes);
        let raw_budget = encoded_budget / 4 * 3;
        let remaining = record.bytes.saturating_sub(offset);
        let to_read = remaining.min(raw_budget as u64) as usize;
        let mut file = std::fs::File::open(&record.path).map_err(|error| {
            NativeFailure::new(
                "AGENTSHIM_READ_IO_FAILED",
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "io", "operation": "artifact_open" })),
            )
        })?;
        file.seek(SeekFrom::Start(offset)).map_err(|error| {
            NativeFailure::new(
                "AGENTSHIM_READ_IO_FAILED",
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "io", "operation": "artifact_seek" })),
            )
        })?;
        let mut bytes = vec![0_u8; to_read];
        file.read_exact(&mut bytes).map_err(|error| {
            NativeFailure::new(
                "AGENTSHIM_READ_IO_FAILED",
                error.to_string(),
                true,
                Some(serde_json::json!({ "kind": "io", "operation": "artifact_read" })),
            )
        })?;
        let next = offset.saturating_add(to_read as u64);
        let mut text = format!(
            "Artifact: {}\nByte range: {offset}..{next} of {}\nEncoding: base64\nOutput:\n{}",
            record.path.display(),
            record.bytes,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        if next < record.bytes {
            use std::fmt::Write as _;
            write!(text, "\nPartial: next_artifact_offset={next}.")
                .expect("writing to a String cannot fail");
        }
        Ok(crate::engine::ToolText {
            text,
            images: Vec::new(),
        })
    }
}
