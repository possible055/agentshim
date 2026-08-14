from typing import Any

from .base import UnsupportedError
from .mcp import CargoMcpAdapter


class FastctxAdapter(CargoMcpAdapter):
    def __init__(self, name: str = "fastctx", config: dict[str, Any] | None = None):
        super().__init__(name, config)

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
            args["offset"] = start_line
        if line_count is not None:
            args["limit"] = line_count
        if pdf_mode is not None or pages is not None:
            raise UnsupportedError("fastctx does not implement PDF read")
        return self.require_client().call_tool("read", args, timeout_s=timeout_s)

    def invoke_grep(
        self,
        path: str,
        pattern: str,
        glob: str | None = None,
        mode: str = "content",
        case: str = "smart",
        fixed_strings: bool = False,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {"path": path, "query": pattern}
        if glob is not None:
            args["glob"] = glob
        if mode == "files":
            args["summary_only"] = True
        return self.require_client().call_tool("grep", args, timeout_s=timeout_s)

    def invoke_glob(
        self,
        path: str,
        pattern: str = "**/*",
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        return self.require_client().call_tool(
            "glob", {"path": path, "pattern": pattern}, timeout_s=timeout_s
        )

    def invoke_bash(
        self,
        command: str,
        timeout_ms: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {"command": command, "login_shell": False}
        if timeout_ms is not None:
            args["timeout_ms"] = timeout_ms
        return self.require_client().call_tool("bash", args, timeout_s=timeout_s)
