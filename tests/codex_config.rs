const EXAMPLES: [(&str, &str); 3] = [
    (
        "Windows",
        include_str!("../config/codex.windows.toml.example"),
    ),
    ("Linux", include_str!("../config/codex.linux.toml.example")),
    ("macOS", include_str!("../config/codex.macos.toml.example")),
];

#[test]
fn codex_examples_enable_parallel_tool_calls() {
    for (platform, example) in EXAMPLES {
        assert!(
            example.contains("supports_parallel_tool_calls = true"),
            "{platform} example must opt codexshim into parallel MCP tool calls"
        );
    }
}
