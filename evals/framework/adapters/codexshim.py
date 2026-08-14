from typing import Any

from .mcp import CargoMcpAdapter, ExternalToolMap


class CodexshimAdapter(CargoMcpAdapter):
    def __init__(self, name: str = "codexshim", config: dict[str, Any] | None = None):
        super().__init__(name, config)

    def supports_pdf_read(self) -> bool:
        return True

    def fallback_tools(self) -> ExternalToolMap:
        return ExternalToolMap(
            read_tool="read",
            grep_tool="grep",
            glob_tool="glob",
            bash_tool="bash",
        )

    def invoke_run_program(
        self,
        program: str,
        args: list[str] | None = None,
        timeout_ms: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        payload: dict[str, Any] = {"program": program, "args": args or []}
        if timeout_ms is not None:
            payload["timeout_ms"] = timeout_ms
        return self.require_client().call_tool("run_program", payload, timeout_s=timeout_s)

    def invoke_read(
        self,
        path: str,
        start_line: int | None = None,
        line_count: int | None = None,
        pdf_mode: str | None = None,
        pages: str | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {"path": path}
        if start_line is not None:
            args["start_line"] = start_line
        if line_count is not None:
            args["line_count"] = line_count
        if pdf_mode is not None:
            args["pdf_mode"] = pdf_mode
        if pages is not None:
            args["pages"] = pages
        return self.require_client().call_tool("read", args, timeout_s=timeout_s)

    def invoke_grep(
        self,
        path: str,
        pattern: str,
        glob: str | None = None,
        mode: str = "content",
        case: str = "smart",
        fixed_strings: bool = False,
        limit: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {
            "path": path,
            "pattern": pattern,
            "mode": mode,
            "case": case,
            "fixed_strings": fixed_strings,
        }
        if glob is not None:
            args["glob"] = glob
        if limit is not None:
            args["limit"] = limit
        return self.require_client().call_tool("grep", args, timeout_s=timeout_s)

    def invoke_glob(
        self,
        path: str,
        pattern: str = "**/*",
        limit: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {"path": path, "pattern": pattern}
        if limit is not None:
            args["limit"] = limit
        return self.require_client().call_tool("glob", args, timeout_s=timeout_s)

    def invoke_bash(
        self,
        command: str,
        timeout_ms: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {"command": command}
        if timeout_ms is not None:
            args["timeout_ms"] = timeout_ms
        return self.require_client().call_tool("bash", args, timeout_s=timeout_s)
