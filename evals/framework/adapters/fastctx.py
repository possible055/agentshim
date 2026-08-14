from typing import Any

from .base import UnsupportedError
from .mcp import CargoMcpAdapter, ExternalToolMap


class FastctxAdapter(CargoMcpAdapter):
    def __init__(self, name: str = "fastctx", config: dict[str, Any] | None = None):
        super().__init__(name, config)

    def extra_process_names(self) -> set[str]:
        return {"fastctx", "fastctx.exe"}

    def fallback_tools(self) -> ExternalToolMap:
        return ExternalToolMap(
            read_tool="inspect_local_file",
            grep_tool="grep",
            glob_tool="glob",
            bash_tool="run",
            read_start_line="offset",
            read_line_count="limit",
            grep_pattern="query",
            grep_summary="summary_only",
            grep_mode=None,
        )

    def invoke_read(
        self,
        path: str,
        start_line: int | None = None,
        line_count: int | None = None,
        pdf_mode: str | None = None,
        pages: str | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        if pdf_mode is not None or pages is not None:
            raise UnsupportedError("fastctx does not implement PDF read")
        return super().invoke_read(
            path,
            start_line=start_line,
            line_count=line_count,
            timeout_s=timeout_s,
        )

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
        if mode in {"count", "files"} and not self.tools.grep_summary and not self.tools.grep_mode:
            raise UnsupportedError("fastctx cannot express full-scan grep summary")
        return super().invoke_grep(
            path,
            pattern,
            glob=glob,
            mode=mode,
            case=case,
            fixed_strings=fixed_strings,
            limit=limit,
            timeout_s=timeout_s,
        )
