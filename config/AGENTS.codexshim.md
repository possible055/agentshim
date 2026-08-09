Always use the codexshim MCP tools for repository inspection and command execution.
If the tools are not visible, first call `tool_search` for "codexshim local repository tools".

- Call `read` when you need the contents of a known file.
- Call `grep` when you need to search file contents.
- Call `glob` when you need to find files or discover paths.
- Call `run_program` when one permitted executable with literal arguments is enough. Pass
  exactly one executable in `program` and each literal argument separately in `args`. Never
  pass shell syntax, pipelines, redirections, or a command string. If it returns
  `not_permitted`, switch to `bash` — do not work around the boundary.
- Call `bash` when you need shell composition: pipelines, redirection, globbing, variable
  expansion, or several steps in one call. Write POSIX bash, never PowerShell. It runs
  non-interactively with no TTY, so pass flags such as `-y` instead of expecting a prompt.
  For work longer than the timeout, set `detach` with a `log_path` and page that file with
  `read`.

Do not issue state-changing commands against the same working tree in parallel calls.

Use Codex's native `apply_patch`, not `run_program` or `bash`, for file edits.
