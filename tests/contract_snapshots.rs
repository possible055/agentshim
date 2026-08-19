use agentshim::{
    AgentShim, NEXT_OFFSET_FIELD, NEXT_START_LINE_FIELD, PARTIAL_MARKER, PDF_CURSOR_FIELD,
    ReadScope,
};
use serde_json::{Value, json};

/// Descriptions are written for a model that can only act on what it can observe, so a
/// server-side setting it cannot read must never appear in one: naming it says "the
/// behaviour of omitting this argument is unknowable to you".
#[test]
fn descriptions_never_reference_server_environment() {
    for scope in [ReadScope::Normal, ReadScope::Unrestricted] {
        let catalog = serialized_catalog(scope);
        assert!(
            !catalog.contains("AGENTSHIM_"),
            "descriptions must not name server environment variables: {catalog}"
        );
    }
}

/// The descriptions instruct the caller to copy continuation values out of the response
/// verbatim, which is only true while the renderers still emit those exact field names.
/// Both ends are pinned to the same constants, so renaming one without the other fails
/// here rather than silently leaving the caller following an instruction that no longer
/// matches any output.
#[test]
fn descriptions_quote_real_continuation_markers() {
    for scope in [ReadScope::Normal, ReadScope::Unrestricted] {
        let result =
            serde_json::to_value(AgentShim::tools_result_for(scope)).expect("serialize tools");
        let tools = result["tools"].as_array().expect("tools array");

        for (name, field) in [
            ("read", NEXT_START_LINE_FIELD),
            ("grep", NEXT_OFFSET_FIELD),
            ("glob", NEXT_OFFSET_FIELD),
        ] {
            let rendered = tool(tools, name).to_string();
            assert!(
                rendered.contains(PARTIAL_MARKER),
                "{name} must tell the caller how to recognise a truncated response"
            );
            assert!(
                rendered.contains(field),
                "{name} must name the {field} argument the renderer actually emits"
            );
        }

        assert!(
            tool(tools, "read").to_string().contains(PDF_CURSOR_FIELD),
            "read must name the PDF continuation argument the renderer actually emits"
        );
    }
}

/// Every tool description, argument description, and title as one string. Scanning the
/// whole catalog rather than walking it field by field keeps the check total: a leak into
/// a field nobody thought to visit still fails.
fn serialized_catalog(scope: ReadScope) -> String {
    serde_json::to_string(&AgentShim::tools_result_for(scope)).expect("serialize tools")
}

#[test]
fn server_discover_advertises_supported_versions_and_tool_capability() {
    let discover = serde_json::to_value(AgentShim::discovery_result()).expect("serialize discover");
    assert_eq!(
        discover["supportedVersions"],
        json!([
            "2026-07-28",
            "2025-11-25",
            "2025-06-18",
            "2025-03-26",
            "2024-11-05"
        ])
    );
    assert_eq!(discover["capabilities"], json!({ "tools": {} }));
}

#[test]
fn successful_tools_do_not_advertise_output_schemas() {
    let result = serde_json::to_value(AgentShim::tools_result()).expect("serialize tools");
    let tools = result["tools"].as_array().expect("tools array");
    for name in ["read", "grep", "glob", "run_program", "bash", "bash_status"] {
        assert!(
            tool(tools, name).get("outputSchema").is_none(),
            "{name} must not advertise an output schema"
        );
    }
}

#[test]
fn tool_annotations_match_codex_approval_contract() {
    let result = serde_json::to_value(AgentShim::tools_result()).expect("serialize tools");
    let tools = result["tools"].as_array().expect("tools array");

    for name in ["read", "grep", "glob", "bash_status"] {
        assert_eq!(
            tool(tools, name)["annotations"],
            json!({
                "readOnlyHint": true,
                "destructiveHint": false,
                "openWorldHint": false
            })
        );
    }

    for name in ["run_program", "bash"] {
        assert_eq!(
            tool(tools, name)["annotations"],
            json!({
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            })
        );
    }
}

#[test]
fn unrestricted_catalog_keeps_approval_annotations() {
    let normal = serde_json::to_value(AgentShim::tools_result()).expect("normal tools");
    let unrestricted = serde_json::to_value(AgentShim::tools_result_for(ReadScope::Unrestricted))
        .expect("unrestricted tools");
    let normal_tools = normal["tools"].as_array().expect("normal tools array");
    let unrestricted_tools = unrestricted["tools"]
        .as_array()
        .expect("unrestricted tools array");

    for name in ["read", "grep", "glob", "run_program", "bash", "bash_status"] {
        assert_eq!(
            tool(normal_tools, name)["annotations"],
            tool(unrestricted_tools, name)["annotations"]
        );
    }
}

#[test]
fn bash_job_tools_have_fixed_order_and_mutually_exclusive_schemas() {
    let result = serde_json::to_value(AgentShim::tools_result()).expect("serialize tools");
    let tools = result["tools"].as_array().expect("tools array");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect::<Vec<_>>(),
        ["read", "grep", "glob", "run_program", "bash", "bash_status"]
    );

    let bash_schema = &tool(tools, "bash")["inputSchema"];
    let alternatives = bash_schema["oneOf"].as_array().expect("bash oneOf");
    assert_eq!(alternatives.len(), 3);
    assert_eq!(alternatives[0]["required"], json!(["command"]));
    assert_eq!(
        alternatives[1]["required"],
        json!(["command", "detach", "log_path"])
    );
    assert_eq!(alternatives[1]["properties"]["detach"]["const"], true);
    assert_eq!(alternatives[2]["required"], json!(["action", "job_id"]));
    assert_eq!(
        alternatives[2]["properties"]["action"]["const"],
        "terminate"
    );
    assert_eq!(alternatives[0]["additionalProperties"], false);
    assert_eq!(alternatives[1]["additionalProperties"], false);
    assert_eq!(alternatives[2]["additionalProperties"], false);

    let status_schema = &tool(tools, "bash_status")["inputSchema"];
    assert_eq!(status_schema["required"], json!(["job_id"]));
    assert_eq!(status_schema["additionalProperties"], false);
    assert_eq!(status_schema["properties"]["tail_bytes"]["minimum"], 0);
    assert_eq!(status_schema["properties"]["tail_bytes"]["maximum"], 16384);
    assert_eq!(status_schema["properties"]["tail_bytes"]["default"], 8192);
    assert_eq!(status_schema["properties"]["wait_ms"]["maximum"], 1000);
}

fn tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("missing tool {name}"))
}
