use codexshim::path::ReadScope;
use codexshim::server::CodexShim;
use serde_json::{Value, json};

fn assert_snapshot(actual: impl serde::Serialize, expected: &str) {
    let actual = serde_json::to_value(actual).expect("serialize snapshot");
    let expected: serde_json::Value = serde_json::from_str(expected).expect("parse snapshot");
    assert_eq!(actual, expected);
}

#[test]
fn server_discover_snapshot() {
    assert_snapshot(
        CodexShim::discovery_result(),
        include_str!("snapshots/server_discover.json"),
    );
}

#[test]
fn tools_list_snapshot() {
    assert_snapshot(
        CodexShim::tools_result(),
        include_str!("snapshots/tools_list.json"),
    );
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

    assert_eq!(
        tool(tools, "run_process")["annotations"],
        json!({
            "readOnlyHint": false,
            "destructiveHint": true,
            "idempotentHint": false,
            "openWorldHint": true
        })
    );
}

#[test]
fn unrestricted_catalog_changes_scope_text_without_changing_approval_annotations() {
    let repository = serde_json::to_value(CodexShim::tools_result()).expect("repository tools");
    let unrestricted = serde_json::to_value(CodexShim::tools_result_for(ReadScope::Unrestricted))
        .expect("unrestricted tools");
    let repository_tools = repository["tools"]
        .as_array()
        .expect("repository tools array");
    let unrestricted_tools = unrestricted["tools"]
        .as_array()
        .expect("unrestricted tools array");

    for name in ["read", "grep", "glob", "run_process"] {
        assert_eq!(
            tool(repository_tools, name)["annotations"],
            tool(unrestricted_tools, name)["annotations"]
        );
    }
    for name in ["read", "grep", "glob"] {
        assert!(
            tool(unrestricted_tools, name)["description"]
                .as_str()
                .expect("description")
                .contains("local filesystem")
        );
    }
    assert_eq!(
        tool(repository_tools, "run_process"),
        tool(unrestricted_tools, "run_process")
    );
}

fn tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("missing tool {name}"))
}
