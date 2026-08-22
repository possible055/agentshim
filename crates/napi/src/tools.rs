//! The three text tools' argument shapes and their engine-side execution.

use std::sync::Arc;

use napi::{Env, Error, Result, Unknown};

use crate::engine::{
    Engine, EngineState, NativeImage, NativeToolTextResult, ToolText, detached_native_work,
    native_promise,
};
use crate::failures::{
    filter_capture_glob_lines, glob_failure, grep_failure, parse_grep_case, parse_grep_mode,
    read_failure,
};
use crate::process::napi_failure;

impl Engine {
    fn pdf_mode(value: Option<&str>) -> Result<Option<agentshim_core::tools::read::PdfMode>> {
        match value {
            None => Ok(None),
            Some("auto") => Ok(Some(agentshim_core::tools::read::PdfMode::Auto)),
            Some("text") => Ok(Some(agentshim_core::tools::read::PdfMode::Text)),
            Some("image") => Ok(Some(agentshim_core::tools::read::PdfMode::Image)),
            Some(other) => Err(Error::new(
                napi::Status::InvalidArg,
                format!("pdf_mode must be auto, text, or image, got {other}"),
            )),
        }
    }

    /// Shared entry for the three text tools: resolve the engine state, and for a
    /// dead engine still return a promise carrying the failure rather than throwing.
    pub(crate) fn tool_text_promise<F, Fut>(
        &self,
        env: Env,
        tool: &'static str,
        run: F,
    ) -> Result<Unknown<'static>>
    where
        F: FnOnce(Arc<EngineState>) -> Fut + Send + 'static,
        Fut: std::future::Future<
                Output = std::result::Result<ToolText, crate::process::NativeFailure>,
            > + Send
            + 'static,
    {
        let state = match self.state() {
            Ok(state) => state,
            Err(error) => {
                let result = NativeToolTextResult {
                    value: None,
                    failure: Some(napi_failure(tool, error)),
                };
                return native_promise(env, detached_native_work(), async move { Ok(result) });
            }
        };
        let work = state.start_native_work();
        native_promise(env, work, async move {
            Ok(match run(state).await {
                Ok(value) => NativeToolTextResult {
                    value: Some(value),
                    failure: None,
                },
                Err(error) => NativeToolTextResult {
                    value: None,
                    failure: Some(error),
                },
            })
        })
    }
    pub(crate) async fn read_text_inner(
        state: Arc<EngineState>,
        call_id: String,
        args: ReadArgs,
    ) -> std::result::Result<ToolText, crate::process::NativeFailure> {
        let cancellation = state.call_token(&call_id)?;
        let artifact = state.artifact(&args.path);
        if artifact.is_none() && state.is_capture_path(&args.path) {
            return Err(crate::process::NativeFailure::invalid(
                "capture files require an exact artifact capability from this Engine",
            ));
        }
        if args.artifact_offset.is_some() && artifact.is_none() {
            return Err(crate::process::NativeFailure::invalid(
                "artifactOffset applies only to a published native artifact",
            ));
        }
        if let Some(record) = artifact.as_ref()
            && (!record.valid_text || args.artifact_offset.is_some())
        {
            return state.read_artifact_page(record, args.artifact_offset);
        }
        let access = state.granted_access(artifact.as_ref(), "AGENTSHIM_READ_PATH_FAILED")?;
        let tool_engine = state.engine_with_access(access, "AGENTSHIM_READ_PATH_FAILED")?;
        let request = agentshim_core::tools::read::ReadRequest {
            path: args.path,
            start_line: args.start_line.map(|line| line as usize),
            line_count: args.line_count.map(|count| count as usize),
            encoding: args.encoding,
            pdf_mode: Self::pdf_mode(args.pdf_mode.as_deref())
                .map_err(|error| napi_failure("read", error))?,
            pages: args.pages,
            pdf_cursor: args.pdf_cursor,
        };
        let output = tool_engine
            .read(
                request,
                agentshim_core::OperationContext::new(
                    cancellation,
                    Arc::new(state.output_limits.clone()),
                ),
            )
            .await
            .map_err(read_failure)?;
        Ok(ToolText {
            text: output.text,
            images: output
                .images
                .into_iter()
                .map(|image| NativeImage {
                    data: image.data,
                    mime_type: image.mime_type.to_owned(),
                })
                .collect(),
        })
    }

    pub(crate) async fn grep_text_inner(
        state: Arc<EngineState>,
        call_id: String,
        args: GrepArgs,
    ) -> std::result::Result<ToolText, crate::process::NativeFailure> {
        use agentshim_core::tools::grep;
        let cancellation = state.call_token(&call_id)?;
        let artifact = args.path.as_deref().and_then(|path| state.artifact(path));
        if artifact.is_none()
            && args
                .path
                .as_deref()
                .is_some_and(|path| state.is_capture_path(path))
        {
            return Err(crate::process::NativeFailure::invalid(
                "capture files require an exact artifact capability from this Engine",
            ));
        }
        if let Some(record) = artifact.as_ref() {
            if args.glob.is_some() {
                return Err(crate::process::NativeFailure::invalid(
                    "artifact grep requires one exact file path and no glob",
                ));
            }
            if !record.valid_text {
                return Err(crate::process::NativeFailure::invalid(
                    "binary artifact cannot be searched as text; retry read with artifactOffset",
                ));
            }
        }
        let access = state.granted_access(artifact.as_ref(), "AGENTSHIM_GREP_PATH_FAILED")?;
        let tool_engine = state.engine_with_access(access, "AGENTSHIM_GREP_PATH_FAILED")?;
        let mode =
            parse_grep_mode(args.mode.as_deref()).map_err(|error| napi_failure("grep", error))?;
        let case =
            parse_grep_case(args.case.as_deref()).map_err(|error| napi_failure("grep", error))?;
        let request = grep::GrepRequest {
            pattern: args.pattern,
            path: args.path,
            glob: args.glob,
            mode,
            fixed_strings: args.fixed_strings,
            case,
            context_lines: args.context_lines.map(|lines| lines as usize),
            offset: args.offset.map(|offset| offset as usize),
            limit: args.limit.map(|limit| limit as usize),
            include_ignored: args.include_ignored,
            encoding: args.encoding,
            fallback_encoding: args.fallback_encoding,
        };
        let text = tool_engine
            .grep(
                request,
                agentshim_core::OperationContext::new(
                    cancellation,
                    Arc::new(state.output_limits.clone()),
                ),
            )
            .await
            .map_err(grep_failure)?
            .text;
        Ok(ToolText {
            text,
            images: Vec::new(),
        })
    }

    pub(crate) async fn glob_text_inner(
        state: Arc<EngineState>,
        call_id: String,
        args: GlobArgs,
    ) -> std::result::Result<ToolText, crate::process::NativeFailure> {
        use agentshim_core::tools::glob;
        let cancellation = state.call_token(&call_id)?;
        if args
            .path
            .as_deref()
            .is_some_and(|path| state.is_capture_path(path))
        {
            return Err(crate::process::NativeFailure::invalid(
                "glob cannot enumerate the capture root",
            ));
        }
        let entry_type = match args.entry_type.as_deref() {
            None => None,
            Some("file") => Some(agentshim_core::tools::glob::GlobEntryType::File),
            Some("directory") => Some(agentshim_core::tools::glob::GlobEntryType::Directory),
            Some("any") => Some(agentshim_core::tools::glob::GlobEntryType::Any),
            Some(other) => {
                return Err(crate::process::NativeFailure::invalid(format!(
                    "type must be file, directory, or any, got {other}"
                )));
            }
        };
        let request = glob::GlobRequest {
            pattern: args.pattern,
            path: args.path,
            include_ignored: args.include_ignored,
            entry_type,
            offset: args.offset.map(|offset| offset as usize),
            limit: args.limit.map(|limit| limit as usize),
        };
        let repository_root = state.root.path().to_path_buf();
        let capture_root = state.capture_root.clone();
        let output = state
            .tool_engine
            .glob(
                request,
                agentshim_core::OperationContext::new(
                    cancellation,
                    Arc::new(state.output_limits.clone()),
                ),
            )
            .await
            .map_err(glob_failure)?;
        let text = filter_capture_glob_lines(&output.text, &repository_root, &capture_root);
        Ok(ToolText {
            text,
            images: Vec::new(),
        })
    }
}

#[napi_derive::napi(object)]
pub struct ReadArgs {
    pub path: String,
    pub encoding: Option<String>,
    pub start_line: Option<u32>,
    pub line_count: Option<u32>,
    pub pages: Option<String>,
    pub pdf_mode: Option<String>,
    pub pdf_cursor: Option<String>,
    pub artifact_offset: Option<f64>,
}

#[napi_derive::napi(object)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub mode: Option<String>,
    pub fixed_strings: Option<bool>,
    pub case: Option<String>,
    pub context_lines: Option<u32>,
    pub offset: Option<u32>,
    pub limit: Option<u32>,
    pub include_ignored: Option<bool>,
    pub encoding: Option<String>,
    pub fallback_encoding: Option<String>,
}

#[napi_derive::napi(object)]
pub struct GlobArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub include_ignored: Option<bool>,
    pub entry_type: Option<String>,
    pub offset: Option<u32>,
    pub limit: Option<u32>,
}
