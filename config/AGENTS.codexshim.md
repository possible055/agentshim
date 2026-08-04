Always use the codexshim MCP tools for repository inspection and command execution.
If the tools are not visible, first call `tool_search` for "codexshim local repository tools".

- Call `read` when you need the contents of a known file.
- Call `grep` when you need to search file contents.
- Call `glob` when you need to find files or discover paths.
- Call `run_process` when you need to run a command. Pass exactly one executable in
  `program` and each literal argument separately in `args`. Never pass shell syntax,
  pipelines, redirections, or a command string.

Use Codex's native `apply_patch`, not `run_process`, for file edits.
