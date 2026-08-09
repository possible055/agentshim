#[cfg(test)]
mod tests {
    use std::fs;

    use rmcp::model::{CallToolRequestParams, CallToolResponse, ContentBlock};
    use serde_json::json;

    use super::{
        CodexShim, ProtocolCompatibility, ToolAdmission, ToolAdmissionFailure, blocking_response,
        diagnostic_tool_error, queue_timeout_message, tool_error,
    };
    use crate::output::MODEL_BYTE_LIMIT;

    #[test]
    fn protocol_compatibility_accepts_only_explicit_levels() {
        assert_eq!(
            ProtocolCompatibility::default(),
            ProtocolCompatibility::Legacy
        );
        assert_eq!(
            "strict".parse::<ProtocolCompatibility>().expect("strict"),
            ProtocolCompatibility::Strict
        );
        assert_eq!(
            "legacy".parse::<ProtocolCompatibility>().expect("legacy"),
            ProtocolCompatibility::Legacy
        );
        assert!("auto".parse::<ProtocolCompatibility>().is_err());
        assert!("LEGACY".parse::<ProtocolCompatibility>().is_err());
    }

    #[test]
    fn process_queue_timeout_does_not_claim_process_diagnostics() {
        let message = queue_timeout_message("run_program", 25);
        assert!(message.contains("no child was started"));
        for field in ["Resolved program:", "Launcher:", "Cwd:", "Exit code:"] {
            assert!(!message.contains(field));
        }
    }

    #[test]
    fn tool_errors_are_bounded() {
        let CallToolResponse::Complete(result) =
            tool_error("validation", false, "界".repeat(40_000), None)
        else {
            panic!("tool error must be complete");
        };
        let ContentBlock::Text(content) = &result.content[0] else {
            panic!("tool error must contain text");
        };
        let text = &content.text;
        assert!(text.ends_with("...[truncated]"));
        assert!(text.len() <= MODEL_BYTE_LIMIT);

        let structured = result
            .structured_content
            .as_ref()
            .expect("tool error must contain structured content");
        assert_eq!(structured["error"]["code"], "validation");
        assert_eq!(structured["error"]["retryable"], false);
        assert_eq!(structured["error"]["message"], text.as_str());
        assert_eq!(result.is_error, Some(true));
        assert!(crate::output::tool_result_fits_budget(text, Some(structured), true));
        assert!(
            crate::output::tool_result_encoded_len(text, Some(structured), true) <= MODEL_BYTE_LIMIT
        );
    }

    #[test]
    fn tool_error_budget_counts_escaped_text_and_bounds_detail_captures() {
        let details = json!({
            "stdout": { "text": "\\\"\u{1}".repeat(20_000), "total_bytes": 60_000 },
            "termination_outcome": "terminated"
        });
        let CallToolResponse::Complete(result) = tool_error(
            "resource_timeout",
            true,
            "\\\"\u{1}".repeat(20_000),
            Some(&details),
        ) else {
            panic!("tool error must be complete");
        };
        let ContentBlock::Text(content) = &result.content[0] else {
            panic!("tool error must contain text");
        };
        let structured = result.structured_content.as_ref().expect("structured error");
        assert!(content.text.ends_with("...[truncated]"));
        assert_eq!(
            structured["error"]["details"]["termination_outcome"],
            "terminated"
        );
        assert!(crate::output::tool_result_fits_budget(
            &content.text,
            Some(structured),
            true
        ));
    }

    #[test]
    fn successful_tool_responses_omit_structured_content() {
        let output =
            crate::tools::ToolOutput::with_child_nonzero("summary".to_owned(), true);
        let CallToolResponse::Complete(result) =
            blocking_response::<crate::tools::exec::ProcessError>(
                "run_program",
                3,
                Ok(Ok(output)),
            )
        else {
            panic!("tool response must be complete");
        };

        let ContentBlock::Text(content) = &result.content[0] else {
            panic!("success response must contain text");
        };
        assert_eq!(content.text, "summary");
        assert_eq!(result.structured_content, None);
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn unavailable_bash_response_is_io_and_not_retryable() {
        let error = crate::tools::exec::ProcessError::Unavailable("no GNU bash".to_owned());
        let CallToolResponse::Complete(result) = diagnostic_tool_error(&error) else {
            panic!("tool error must be complete");
        };
        let structured = result
            .structured_content
            .as_ref()
            .expect("structured error");

        assert_eq!(structured["error"]["code"], "io");
        assert_eq!(structured["error"]["retryable"], false);
        assert_eq!(result.is_error, Some(true));
    }

    fn detached_request() -> CallToolRequestParams {
        serde_json::from_value(json!({
            "name": "bash",
            "arguments": {
                "command": "sleep 30",
                "detach": true,
                "log_path": "build.log"
            }
        }))
        .expect("call tool request")
    }

    #[test]
    fn detached_admission_reserves_before_blocking_scheduling_and_fails_fast() {
        let fixture = tempfile::tempdir().expect("fixture");
        let mut runtime = crate::runtime::RuntimeConfig::for_tests(1);
        runtime.detached_calls = 1;
        let server = CodexShim::builder(fixture.path())
            .expect("builder")
            .runtime_limits(runtime)
            .build()
            .expect("server");
        let request = detached_request();
        let first = server
            .try_admit_tool(&request)
            .expect("first detached admission");

        assert_eq!(server.detached.reserved_count(), 1);
        assert!(matches!(
            server.try_admit_tool(&request),
            Err(ToolAdmissionFailure::Process(
                crate::tools::exec::ProcessError::ResourceBusy(_)
            ))
        ));
        drop(first);
        assert_eq!(server.detached.reserved_count(), 0);
    }

    #[test]
    fn foreground_saturation_does_not_consume_detached_capacity() {
        let fixture = tempfile::tempdir().expect("fixture");
        let mut runtime = crate::runtime::RuntimeConfig::for_tests(1);
        runtime.process_calls = 1;
        runtime.detached_calls = 1;
        let server = CodexShim::builder(fixture.path())
            .expect("builder")
            .runtime_limits(runtime)
            .build()
            .expect("server");
        let foreground = server
            .resources
            .try_admit_process()
            .expect("foreground admission");

        assert!(server.resources.try_admit_process().is_none());
        let detached = server
            .try_admit_tool(&detached_request())
            .expect("detached admission remains independent");
        assert!(matches!(detached, ToolAdmission::Detached(_)));
        drop(foreground);
    }

    #[test]
    fn root_capability_blocks_parent_escape() {
        let fixture = tempfile::tempdir().expect("create fixture");
        let root = fixture.path().join("root");
        fs::create_dir(&root).expect("create root");
        fs::write(fixture.path().join("outside.txt"), "outside").expect("write outside");
        let server = CodexShim::from_path(&root).expect("open root");

        let error = server
            .root
            .capability()
            .read_to_string("../outside.txt")
            .expect_err("parent escape must fail");
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
        ));
    }

    #[cfg(unix)]
    #[test]
    fn root_capability_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("create fixture");
        let root = fixture.path().join("root");
        fs::create_dir(&root).expect("create root");
        let outside = fixture.path().join("outside.txt");
        fs::write(&outside, "outside").expect("write outside");
        symlink(&outside, root.join("escape")).expect("create symlink");
        let server = CodexShim::from_path(&root).expect("open root");

        server
            .root
            .capability()
            .read_to_string("escape")
            .expect_err("symlink escape must fail");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn root_handle_preserves_repository_identity() {
        let fixture = tempfile::tempdir().expect("create fixture");
        let root = fixture.path().join("root");
        let moved = fixture.path().join("moved");
        fs::create_dir(&root).expect("create root");
        fs::write(root.join("identity.txt"), "original").expect("write original");
        let server = CodexShim::from_path(&root).expect("open root");

        #[cfg(unix)]
        {
            fs::rename(&root, &moved).expect("move original root");
            fs::create_dir(&root).expect("create replacement root");
            fs::write(root.join("identity.txt"), "replacement").expect("write replacement");
        }
        #[cfg(windows)]
        {
            let error =
                fs::rename(&root, &moved).expect_err("held Windows root blocks replacement");
            assert!(
                matches!(error.raw_os_error(), Some(5 | 32)),
                "unexpected Windows root rename error: {error}"
            );
        }

        assert_eq!(
            server
                .root
                .capability()
                .read_to_string("identity.txt")
                .expect("read held root"),
            "original"
        );
    }
}
