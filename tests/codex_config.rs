const EXAMPLES: [(&str, &str); 3] = [
    (
        "Windows",
        include_str!("../config/codex.windows.toml.example"),
    ),
    ("Linux", include_str!("../config/codex.linux.toml.example")),
    ("macOS", include_str!("../config/codex.macos.toml.example")),
];

const READMES: [(&str, &str); 2] = [
    ("README.md", include_str!("../README.md")),
    ("README.zh-CN.md", include_str!("../README.zh-CN.md")),
];

const CURSOR_EXAMPLE: &str = include_str!("../config/cursor.mcp.json.example");
const CODEX_ARGS: &str = r#"args = ["serve", "--client-profile", "codex"]"#;

/// The settings that must not drift between the examples and the documentation. A stale
/// `tool_timeout_sec` in particular makes the client give up before the server's own ceiling.
const REQUIRED_SETTINGS: [&str; 5] = [
    "required = true",
    "supports_parallel_tool_calls = true",
    "startup_timeout_sec = 15",
    "tool_timeout_sec = 600",
    r#"enabled_tools = ["read", "grep", "glob", "run_program", "bash"]"#,
];

#[test]
fn codex_examples_and_readmes_agree_on_required_settings() {
    for (source, text) in EXAMPLES.iter().chain(READMES.iter()) {
        for setting in REQUIRED_SETTINGS {
            assert!(text.contains(setting), "{source} must document `{setting}`");
        }
        assert!(
            text.contains(CODEX_ARGS),
            "{source} must document `{CODEX_ARGS}`"
        );
        for stale in [
            "run_process",
            "tool_timeout_sec = 310",
            "tool_timeout_sec = 610",
        ] {
            assert!(!text.contains(stale), "{source} still mentions `{stale}`");
        }
    }
}

#[test]
fn cursor_example_is_valid_json_and_selects_cursor_profile() {
    let example: serde_json::Value =
        serde_json::from_str(CURSOR_EXAMPLE).expect("Cursor example must be valid JSON");
    let server = &example["mcpServers"]["codexshim"];
    assert_eq!(server["type"], "stdio");
    assert_eq!(server["command"], "/absolute/path/to/codexshim");
    assert_eq!(
        server["args"],
        serde_json::json!(["serve", "--client-profile", "cursor"])
    );
    for (readme, text) in READMES {
        assert!(
            text.contains("config/cursor.mcp.json.example"),
            "{readme} must link the Cursor example"
        );
    }
}

#[test]
fn codex_examples_declare_both_approval_modes() {
    for (platform, example) in EXAMPLES {
        assert!(
            example.contains("[mcp_servers.codexshim.tools.run_program]")
                && example.contains("[mcp_servers.codexshim.tools.bash]"),
            "{platform} example must configure approval for both execution tools"
        );
    }
}
