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

/// The settings that must not drift between the examples and the documentation. A stale
/// `tool_timeout_sec` in particular makes the client give up before the server's own ceiling.
const REQUIRED_SETTINGS: [&str; 4] = [
    "supports_parallel_tool_calls = true",
    "tool_timeout_sec = 600",
    r#"enabled_tools = ["read", "grep", "glob", "run_program", "bash"]"#,
    r#"args = ["serve"]"#,
];

#[test]
fn codex_examples_and_readmes_agree_on_required_settings() {
    for (source, text) in EXAMPLES.iter().chain(READMES.iter()) {
        for setting in REQUIRED_SETTINGS {
            assert!(text.contains(setting), "{source} must document `{setting}`");
        }
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
fn codex_examples_declare_both_approval_modes() {
    for (platform, example) in EXAMPLES {
        assert!(
            example.contains("[mcp_servers.codexshim.tools.run_program]")
                && example.contains("[mcp_servers.codexshim.tools.bash]"),
            "{platform} example must configure approval for both execution tools"
        );
    }
}
