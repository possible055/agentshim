Always use the codexshim MCP tools for repository inspection and command execution.
If the tools are not visible, first call `tool_search` for "codexshim local repository tools".

- Call `read` when you need the contents of a known file.
- Call `grep` when you need to search file contents.
- Call `glob` when you need to find files or discover paths.
- Call `run_program` by default when one executable with literal arguments is enough.
- Call `bash` only when you need shell composition: pipelines, redirection, globbing, variable expansion, or several steps in one call. Write POSIX bash, never PowerShell.

Do not issue state-changing commands against the same working tree in parallel calls.

Use Codex's native `apply_patch`, not `run_program` or `bash`, for file edits.
