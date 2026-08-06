#[cfg(test)]
mod tests {
    use std::fs;

    use rmcp::model::{CallToolResponse, ContentBlock};
    use serde_json::json;

    use super::{
        CodexShim, ProtocolCompatibility, blocking_response, process_queue_timeout_message,
        tool_error,
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
        let message = process_queue_timeout_message(25);
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
        assert!(crate::output::tool_result_fits_budget(text, structured, true));
        assert!(
            crate::output::tool_result_encoded_len(text, structured, true) <= MODEL_BYTE_LIMIT
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
            structured,
            true
        ));
    }

    #[test]
    fn success_response_uses_one_typed_result_for_text_and_structured_content() {
        let output = crate::tools::ToolOutput::new(
            "summary".to_owned(),
            &json!({"path": "src/lib.rs", "complete": true}),
        )
        .expect("create output");
        let CallToolResponse::Complete(result) =
            blocking_response::<crate::tools::read::ReadError>("read", 3, Ok(Ok(output)))
        else {
            panic!("tool response must be complete");
        };

        let ContentBlock::Text(content) = &result.content[0] else {
            panic!("success response must contain text");
        };
        assert_eq!(content.text, "summary");
        assert_eq!(
            result.structured_content,
            Some(json!({"path": "src/lib.rs", "complete": true}))
        );
        assert_eq!(result.is_error, Some(false));
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
