use codexshim::{CodexShim, ReadScope};
use serde_json::{Value, json};

#[test]
fn server_discover_advertises_supported_versions_and_tool_capability() {
    let discover = serde_json::to_value(CodexShim::discovery_result()).expect("serialize discover");
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
    let result = serde_json::to_value(CodexShim::tools_result()).expect("serialize tools");
    let tools = result["tools"].as_array().expect("tools array");
    for name in ["read", "grep", "glob", "run_program", "bash"] {
        assert!(
            tool(tools, name).get("outputSchema").is_none(),
            "{name} must not advertise an output schema"
        );
    }
}

#[test]
fn tool_annotations_match_codex_approval_contract() {
    let result = serde_json::to_value(CodexShim::tools_result()).expect("serialize tools");
    let tools = result["tools"].as_array().expect("tools array");

    for name in ["read", "grep", "glob"] {
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
    let normal = serde_json::to_value(CodexShim::tools_result()).expect("normal tools");
    let unrestricted = serde_json::to_value(CodexShim::tools_result_for(ReadScope::Unrestricted))
        .expect("unrestricted tools");
    let normal_tools = normal["tools"].as_array().expect("normal tools array");
    let unrestricted_tools = unrestricted["tools"]
        .as_array()
        .expect("unrestricted tools array");

    for name in ["read", "grep", "glob", "run_program", "bash"] {
        assert_eq!(
            tool(normal_tools, name)["annotations"],
            tool(unrestricted_tools, name)["annotations"]
        );
    }
}

fn tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("missing tool {name}"))
}
